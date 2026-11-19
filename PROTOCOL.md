# Prostir protocol, version 2

This document describes how the PC program (`host/`) and the Meta Quest app (`client/`)
talk to each other. It is meant for anyone who wants to understand the connection or
write their own client or host.

The source of truth is the code: [`proto/src/lib.rs`](proto/src/lib.rs) (messages),
[`proto/src/wire.rs`](proto/src/wire.rs) (how values are encoded),
[`proto/src/packet.rs`](proto/src/packet.rs) (packets),
[`proto/src/handshake.rs`](proto/src/handshake.rs) (the version handshake on records),
[`proto/src/link/tcp.rs`](proto/src/link/tcp.rs) (encryption) and
[`proto/src/secure.rs`](proto/src/secure.rs) (keys and pairing).

Version 2 (Prostir 0.2) is a clean break from version 1 (Prostir 0.1.x): only the
version handshake is the same, so the two can tell the user which side to update
(see [The version handshake](#the-version-handshake-frozen)). Behaviour that comes from `host/` or `client/` rather than from `proto/` is
what the current apps do; it is marked **implementation-defined** where another
implementation may reasonably differ.

Words used below:

- **host**: the PC program. It listens for connections.
- **client** or **headset**: the Quest app. It connects to the host.
- **stream**: one encoded video picture (a virtual monitor, a window panel or the
  mirrored laptop screen), identified by a one-byte stream id.
- **frozen**: kept byte for byte in every protocol version, past and future.

Contents:

1. [Overview](#1-overview)
2. [Security](#2-security)
3. [Framing](#3-framing)
4. [Session flow](#4-session-flow)
5. [Message reference](#5-message-reference)
6. [Streams, sound and microphone](#6-streams-sound-and-microphone)
7. [Layout](#7-layout)

## 1. Overview

### Transport

| What | Value |
|---|---|
| Connection | TCP, client connects to host |
| Host TCP port | `47830` (`TCP_PORT`; the host's `--port` option changes it) |
| Discovery | UDP broadcast to port `47831` (`DISCOVERY_PORT`) |
| Sessions | One at a time |

The connection is plain TCP carrying encrypted records (see [Security](#2-security)
and [Framing](#3-framing)). There are two ways to reach the host:

- **Wi-Fi or Ethernet**: the client connects to the host's address on the local network.
- **USB**: the host keeps `adb reverse tcp:47830 tcp:47830` set up for every headset
  attached over USB (re-checked every 3 s), so the client reaches the host at
  `127.0.0.1:47830`. The protocol is the same. The host's `--no-adb` option turns this
  off.

### Discovery

Every second the host sends a UDP datagram (the *beacon*) from an ephemeral port to
port `47831` at `255.255.255.255` and at the broadcast address of every IPv4 interface
that is up and not loopback. The `--no-discovery` option turns this off.

Beacon layout:

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | `PROSTIR1` in ASCII (`BEACON_MAGIC`) |
| 8 | 2 | TCP port of the host, `u16` little-endian |
| 10 | rest | Host name, UTF-8, no terminator and no length prefix |

The host name comes from `/proc/sys/kernel/hostname`. A receiver takes the TCP address
as the beacon's source IP plus the port in the beacon. Datagrams that do not start with
the magic are ignored. Beacons are neither encrypted nor authenticated: they are only a
hint where to connect, and trust comes from the handshake.

How the current client picks a host (implementation-defined): `127.0.0.1:47830` (USB)
first; then a PC picked in the menu; it listens for beacons for 1.2 s (a PC counts as
present for 6 s after its last beacon); then an address from a user-provided file, then
the last host that completed a session; then the only PC heard, if exactly one; and
every fifth round it probes port 47830 on every address of its own `/24` network,
because some networks drop broadcasts.

### One session at a time

The host accepts connections one by one in its main loop. While a connection is being
set up, paired or served, further connections wait in the TCP listen queue and are
accepted only after it ends.

## 2. Security

### Noise handshake

Every connection starts with a Noise handshake, before any protocol message:

```
Noise_XX_25519_AESGCM_BLAKE2s
```

- Pattern `XX`: mutual authentication; both static keys travel encrypted.
- DH: Curve25519. Cipher: AES-256-GCM. Hash: BLAKE2s.
- The client (the side that connects) is the **initiator**, the host the **responder**.
- No prologue and no pre-shared key. All three handshake messages carry an empty payload.

```
client -> host   e
host -> client   e, ee, s, es
client -> host   s, se
```

Each handshake message is sent as a 2-byte **big-endian** length followed by the Noise
message (at most 65535 bytes). After the third message both sides switch to transport
mode (see [Records](#records)).

The implementation uses the Rust crate `snow` 0.10.

### Static keys and identity

Each device has one long-term Curve25519 key pair (its *identity*), created on first use.

- Host: `~/.config/prostir/identity` (or `$XDG_CONFIG_HOME/prostir/identity`), one line
  `<private hex> <public hex>`, mode `0600`.
- Headset: a file of the same format inside the app's storage.

A device is identified only by its 32-byte public key, which the handshake reveals to
the other side.

### Trust

Each side keeps a *trust store*: one line per paired peer, `<public key hex> <label>`,
written atomically with mode `0600`. A peer is trusted if and only if its static public
key from the handshake is in the store.

- Host: `~/.config/prostir/paired_headsets`. The label is the `name` from `PairBegin`
  (or `headset` if empty). The tray item "Forget paired headsets" clears the file.
- Headset: a file inside the app. The label is the host's name (from the beacon), its
  IP address, or "PC (USB)". Settings → PC lists and forgets entries.

The host serves only trusted headsets. Before trust it sends nothing but `Welcome`
(its protocol version and interface language), `PairingRequired`, `PairMsg`,
`PairResult`, `Error` and `Incompatible`, and it creates no monitors. The current client pairs with an untrusted host only after the user agreed
in the menu (the agreement holds for 10 minutes); otherwise it closes the connection
right after the handshake and asks the user (implementation-defined).

### Pairing with a 6-digit code

Pairing runs inside the encrypted connection, after the handshake. One side shows a
random 6-digit code, the user types it on the other side, and both sides run SPAKE2
with it. The client chooses the direction in `PairBegin`:

| `headset_shows` | Who makes and shows the code | Who asks for it |
|---|---|---|
| `1` | Headset (in its menu) | Host, in a dialog (`kdialog`, `zenity`, `fuzzel`, `wofi` or `rofi`, the first one found) |
| `0` | Host (tray, desktop notification, log) | Headset, in its menu |

**Code.** Six decimal digits: a random `u32` modulo 1,000,000, zero-padded. It is
displayed as `123 456`. Before use, every character that is not an ASCII digit is
removed, and the remaining ASCII digits are the SPAKE2 password. A code made by the
host is reused for 5 minutes, across attempts and connections, so the code on screen
does not change while the user types it.

**SPAKE2.** Ed25519 group, *symmetric* mode, identity string `prostir pairing v1`, as
implemented by the Rust crate `spake2` 0.4 (compatible with `python-spake2`). Each
side's message is 33 bytes (`0x53` followed by a 32-byte point); the derived key is 32
bytes. With different codes the keys differ, which shows up in the confirmation step.

**Confirmation tags.** Each side proves it holds the same key, bound to this
connection:

```
tag(role) = HMAC-SHA256(key = SPAKE2 key,
                        "prostir confirm v1" || role || handshake_hash || lo || hi)
```

- `role` is the ASCII string `headset` for the client's tag, `pc` for the host's.
- `handshake_hash` is the Noise handshake hash of this connection (32 bytes for BLAKE2s).
- `lo` and `hi` are the two static public keys (32 bytes each), sorted in ascending
  byte order, so both sides compute the same value.
- Tags are 32 bytes and compared in constant time.

Because the tag covers the handshake hash and both static keys, a man in the middle
who runs two separate handshakes cannot relay a valid confirmation.

**Message sequence** (after `Hello` and `Welcome`, before the session is open; the
client starts it when it does not trust the host, or when the host answered `Open`
with `PairingRequired`):

```
client                                   host
  PairBegin{headset_shows, name}  ->
                                         obtains the code (dialog or its own)
                                  <-     PairMsg{host SPAKE2 message}
  PairMsg{client SPAKE2 message}  ->
  PairConfirm{tag("headset")}     ->
                                         checks the tag
                                  <-     PairResult{tag("pc") if the codes matched, else none}
  checks the host's tag
```

The two `PairMsg` messages are independent, so their order on the wire does not
matter: the current client sends its own as soon as it has the code. The host reads
the client's `PairMsg` and then `PairConfirm`; any other message in their place closes
the connection.

On success both sides add the other's static key to their trust store. The connection
stays open, and the client continues with `Open`.

If the host cannot obtain a code (the dialog was cancelled or timed out after 120 s, or
no dialog program is installed), it sends `Error{text}` and `PairResult` without a tag
and without a `PairMsg`. A client waiting for `PairMsg` treats that `PairResult` as a
failed attempt.

### Limits and timeouts

Host (implementation-defined values from `host/src/pairing.rs`):

| Limit | Value |
|---|---|
| Handshake, `Hello` and `Open` | Must be done within 10 s in total; each read waits at most 5 s; each write at most 5 s |
| While pairing | The deadline and the read timeout become 300 s, and go back to 10 s and 5 s after the attempt |
| `PairBegin` per connection | At most 3; a 4th closes the connection |
| Failed attempts in a row (all connections) | The first 3 are free. After the 4th, the next attempt has to wait 30 s, then 60 s, 120 s, 240 s, 480 s, and at most 900 s. A success resets the count |
| During a wait | `PairBegin` is answered with `Error` ("Too many wrong codes. Try again in N min.") and `PairResult` without a tag, and the connection is closed |

Every attempt that does not succeed counts as a failed one: a wrong code, a host that
has no code to offer (cancelled dialog, no dialog program) and an exchange that breaks
off. When the headset shows the code and the host has none, the host reads and drops the
headset's `PairMsg` (sent right after `PairBegin`), so the next `PairBegin` can follow on
the same connection.

Client (implementation-defined, `client/src/net.rs`): read timeout 10 s, 300 s while
pairing; each write at most 5 s; at most 3 attempts per connection, after which it
stops and asks the user again; it waits up to 300 s for the user to type a code shown
by the host, looking at the connection every half second meanwhile.

For the timeouts in the session see [End of session](#end-of-session).

### What is encrypted

Everything after the Noise handshake: every protocol message in both directions,
including `Hello`, pairing, video, PC sound and the microphone. Not encrypted: the
discovery beacon (magic, port, host name) and the Noise handshake messages themselves
(which carry ephemeral keys in the clear and static keys encrypted, as Noise `XX` does).

## 3. Framing

There are three layers. The transport carries encrypted **records**; each record after
the version handshake is one **packet**; packets carry **messages**, cut into pieces
when they are big, so that something urgent (sound, a menu answer) never waits behind
a whole video frame.

### Records

After the Noise handshake, everything travels in records:

```
+---------------------+---------------------------------------------+
| length u16 BE       | ciphertext = AES-256-GCM(plaintext) + 16-B tag |
+---------------------+---------------------------------------------+
```

- `length` is **big-endian** (unlike everything inside) and counts the ciphertext
  including its 16-byte tag; at most 65535.
- So one record holds at most 65519 bytes of plaintext.
- Nonces are not sent. Each direction has its own Noise cipher state, and the nonce is
  a counter starting at 0 for the first record in that direction and growing by 1 per
  record (standard Noise nonce encoding). There is no rekeying. A record that does not
  decrypt ends the connection.
- No associated data.
- The TCP connection closing between two records is a normal end of session; closing in
  the middle of a record is an error.

The version handshake comes first, in the stream format of protocol 1 (see
[The version handshake](#the-version-handshake-frozen)); every record after it is a
packet.

### Packets

```
+----------+---------+------------------------------------+
| flags u8 | lane u8 | body: the next bytes of a message  |
+----------+---------+------------------------------------+
```

- `flags`: bit 0 `FIRST` (the message's first packet), bit 1 `LAST` (its last one); a
  message in one packet has both. The other bits must be 0.
- `lane`: where the message is put back together. A lane holds at most one message at
  a time: its packets follow one another in that lane, and the next message in the
  lane starts only after the last packet of the one before. Packets of other lanes may
  come in between. A lane says nothing about what a message is (the tag does).
- `body`: the next bytes of the message, at least one. The first packet's body starts
  with the message's 5-byte header (tag and length, see
  [Protocol messages](#protocol-messages)), whole, so the receiver knows the length at
  once. The bodies of a message's packets joined are exactly its bytes.
- Order: the messages of one lane arrive in order; messages of different lanes in the
  order of their last packets.
- The sender chooses the lanes and how big each packet is (the current apps: 16 KiB of
  body, lane `0` for control messages, `1` for PC sound and the microphone, and `2 +
  id` for video stream `id`). A receiver accepts any body size from 1 byte to the size
  of a record and any lane.
- A message with a tag the receiver does not know is put together and skipped.

The transport is reliable and ordered, so a packet out of place is a bug or an attack,
never a loss. Each of these ends the connection:

- flag bits other than `FIRST` and `LAST`, or a record too short for the 2-byte packet
  header;
- a packet with an empty body;
- a packet without `FIRST` in a lane that holds no message;
- `FIRST` in a lane that holds a message;
- a first packet whose body is shorter than the 5-byte message header;
- a message longer than the receiver's limit (below), checked on the first packet,
  before anything is allocated;
- bytes beyond the length the header says;
- `LAST` before the message's end, or a packet that completes the message without
  `LAST`;
- more lanes holding a message at once than the limit.

Limits:

| What | Before `Opened` | In the session |
|---|---|---|
| Payload of a message from the host | 64 KiB | 64 MiB |
| Payload of a message from the headset | 64 KiB | 64 KiB |
| Lanes holding a message being put together at once | 1 | 32 |

Before `Opened` the host may not be trusted yet, and the headset is not trusted until
it pairs, so what either can make the other allocate stays small. The biggest headset
message in use, a slice of microphone sound, is under 1 KiB. A message complete in one
packet takes no lane. The host applies the session column once it has sent `Opened`,
the headset once it has read it.

### Protocol messages

```
+--------+-------------------+---------------------+
| tag u8 | length u32 LE     | payload (length B)  |
+--------+-------------------+---------------------+
```

- `tag`: which message (see [Message reference](#5-message-reference)). Host→client tags
  are 1 to 27, client→host tags 101 to 130.
- `length`: payload length in bytes, **little-endian**, at most the limit above.

### Encoding

A payload is the message's fields, one after the other in the order the reference
lists them, with nothing in between (`proto/src/wire.rs`):

| Type | Bytes |
|---|---|
| `u8`, `u16`, `u32`, `u64` | Little-endian |
| `i32` | Little-endian, two's complement |
| `bool` | One byte, `0` or `1` |
| `opt<T>` | One byte: `0` (none), or `1` followed by a `T` |
| `str(max N)` | A `u16` count of bytes, then that many bytes of UTF-8; at most N bytes |
| `list<T>(max N)` | A `u16` count of items, then the items; at most N items |
| `bytes[N]` | Exactly N bytes, no count |
| `(A, B)` | An `A`, then a `B` |
| struct | Its fields in order |
| enum | A `u8` tag, then the fields of the variant with that tag |
| `str*`, `bytes*` | The rest of the payload, no count (only in the frozen messages and the media messages `Frame`, `Audio`, `Mic`) |

Enum tags are fixed numbers written next to each variant in the code, so reordering
variants never changes the bytes. The only enum inside messages so far is the codec:
`0` H.264, `1` H.265 (HEVC).

Reading is strict, and any of these ends the connection:

- a payload shorter than its fields;
- bytes left over after the last field;
- a `bool` or `opt` byte other than `0` or `1`;
- an enum tag no variant has (an unknown codec, say);
- a string that is not valid UTF-8 (also in the frozen messages);
- a string or list over its limit.

A message with a tag the reader does not know is skipped whole (its length is known),
see [Forward compatibility](#forward-compatibility). Unknown values inside a known
message are not skipped: they are errors.

## 4. Session flow

### Opening

```
client                                         host
  TCP connect                          ->
  Noise XX handshake (3 messages)      <->
  Hello{version, min, app}             ->
                                               not compatible:
                                       <-        Error{text}, Incompatible{protocol, app}, close
                                               compatible:
                                       <-        Welcome{version, language}
  [pairing, if the client does not trust the host and the user agreed]
  Open                                 ->
                                               client not trusted:
                                       <-        PairingRequired
  [pairing, then Open again]
                                               trusted:
                                       <-        Opened
  steady state
```

### The version handshake (frozen)

`Hello`, `Welcome`, `Incompatible` and `Error` keep the bytes they have in version 1 in
every version: the fixed fields, then a UTF-8 text running to the end of the payload
(`str*`). `Hello` is always the client's first message, before pairing or anything
else. So any two versions, older or newer, can finish this exchange and tell the user
which side to update. These four must never change; what a new version needs goes into
other messages.

| Tag | Message | Bytes |
|---:|---|---|
| 101 | `Hello` | `version:u32, min:u32, app:str*` |
| 1 | `Welcome` | `version:u32, language:str*` |
| 17 | `Incompatible` | `protocol:u32, app:str*` |
| 6 | `Error` | `text:str*` |

For example `Hello{version: 2, min: 2, app: "0.2.0"}` is
`65 0d 00 00 00 02 00 00 00 02 00 00 00 30 2e 32 2e 30`.

**In records.** These four go in the stream format of protocol 1, as every version
wrote them: the records joined into one byte stream, the messages one after the other
in it, across record boundaries (an empty record carries nothing here). What decides
where the stream ends:

- the client puts `Hello` in a record of its own, with nothing else in it; once the
  versions fit, the host requires that (a `Hello` that shares its record closes the
  connection);
- the host puts `Welcome` in a record of its own; a `Welcome` that shares its record
  closes the connection;
- the next record from the client after `Hello`, and from the host after `Welcome`, is
  the first [packet](#packets); everything from then on is packets, pairing included;
- `Error` and `Incompatible` end a connection whose versions do not fit, so they stay
  in the stream format; a client reads them across any record boundaries.

**Hello.** The client sends its newest protocol (`version` = `PROTOCOL_VERSION`), the
oldest it still understands (`min` = `MIN_PROTOCOL`) and its app version (for example
`0.2.0`).

**Version check.** The host decides with:

```rust
fn compatible(version: u32, min: u32) -> bool {
    version >= MIN_PROTOCOL && min <= PROTOCOL_VERSION
}
```

where `PROTOCOL_VERSION` and `MIN_PROTOCOL` are the host's own. In this version both are
`2`. There is no negotiated version: if the ranges overlap, both sides speak, and each
skips messages it does not know.

If the check fails, the host sends `Error` with a human-readable text (for clients
older than `Incompatible`), then `Incompatible{protocol: host PROTOCOL_VERSION, app:
host app version}`, and closes the connection. This happens before any trust check or
pairing, so an unpaired client also learns this. The client compares `protocol` with
its own `PROTOCOL_VERSION` to tell which side needs an update. The current client waits
10 s before trying again.

A first message other than `Hello` can only come from a version 1 client (Prostir
0.1.x), which may ask to pair before saying hello. The host answers it the same way
(`Error` saying to update the headset, then `Incompatible`) and closes the connection,
skipping the message unread.

**Welcome.** The versions fit. `version` is the host's `PROTOCOL_VERSION`; `language`
is the PC's interface language, `uk` or `en`, which the headset follows by default.
Nothing else starts yet.

**Trust.** If the client does not trust the host's key, it pairs now (only after the
user agreed in its menu; otherwise it closes the connection and asks the user). Then it
sends `Open`. The host answers `PairingRequired` if it does not trust the client: the
client may pair and send `Open` again. Once the host trusts the client it answers
`Opened`, and the session starts. Until then the host accepts only `Open` and
`PairBegin` (and skips unknown tags), and closes the connection on anything else.

If the host then cannot set up its monitors or encoders, it sends `Error{text}` and
closes the connection.

### What a version 1 peer sees

| Peer | What happens |
|---|---|
| 0.1.x client, trusts the host | Sends `Hello{1, 1}`. The host sends `Error` ("Update Prostir in the headset…") and `Incompatible{2, …}`; the client shows its "update the headset" page. |
| 0.1.x client, pairing first, code shown in the headset (its default) | Sends `PairBegin` and its `PairMsg` first. The host sends the same `Error` and `Incompatible` and closes. The client shows the error text, treats `Incompatible` during pairing as unexpected and reconnects, every few hundred milliseconds while the user's approval holds (10 min). |
| 0.1.x client, pairing first, code shown on the PC | Sends `PairBegin`, then waits for the user to type the code the PC would show, up to 300 s; the host shows none, only its "update the headset" notification, and closes. Once a code is typed the client reads the `Error` text, shows it and reconnects as above; after 300 s without one it gives up the attempt and reconnects. |
| 0.1.x host | Gets `Hello{2, 2}` and answers `Error` ("Update Prostir on the PC…") and `Incompatible{1, …}`; the client shows "update the PC". The host also shows a desktop notification. |

### Steady state

Right after `Opened` the host sends, in this order: `PrimaryChanged` (if a primary
monitor is set), `Order`, `MirrorWanted`, `AudioWanted`, `MicWanted`, `PcSettings`,
`Abilities`. Then it forces a keyframe on every encoder, and each stream starts with
`StreamStart` followed by `Frame`s (see [Streams](#6-streams-sound-and-microphone)).
`Focus` follows whenever the monitor under the mouse changes (checked every 150 ms).

The current client sends after `Opened`: `SetMic` (its microphone switch), and
`SetNotifications{1}` and `SetSoundFromScreen{1}` if those are on, and `Presence{0}`
if the headset is not worn. `SetLayout` follows once the monitors are placed and the
arrangement has held still for 800 ms. Everything else is sent when the user does
something.

Both sides send `Ping{seq}` every second once the session is open (the host from
`Opened` on, the client from reading it), with `seq` counting up from 0, and answer a
`Ping` at once with a `Pong` carrying its `seq`. A peer must answer pings; it may send
its own at another rate or not at all, but then the other side sees silence (below)
when nothing else flows. The current apps answer from their reading thread, so the
round trip measures the network and not a busy main loop, and log it every 10 s.

The host ignores (and logs) known client messages that have no meaning in the session,
such as `Open` or `PairBegin`. The client ignores host pairing messages, `Welcome`,
`Opened` and `Incompatible` in the session.

### End of session

A side that ends a session on purpose sends `Goodbye{reason}` in the control lane,
after the control messages it still had to send and instead of any more sound or
video, and then sends nothing more. The other side closes the connection once it read
it. The side that said goodbye stops sending (a TCP half close) but still reads and
drops what arrives, until the other side closed the connection or for a short while
(1 s in the current apps), and then closes too: closing with unread data would reset
the connection, which can throw away the goodbye on its way (over `adb reverse` it
does). A reason a reader does not know reads as `0`.

| Code | From the host | From the headset |
|---:|---|---|
| `0` | Other | Other |
| `1` | The PC app is closing (tray, signal, the PC shutting down) | The headset app is closing |
| `2` | The headset was not worn for a while; it may connect again once it is worn | The user chose another PC |
| `3` | The PC forgot this headset (the tray's "Forget paired headsets"): pair again | The user forgot this PC in the headset |
| `4` | A new connection of this headset replaced this one | |
| `5` | The monitors or encoders stopped; an `Error` with a text came first | |

What the current client shows: "The PC app was closed" for `1`, the pairing offer
for `3` ("The PC no longer knows this headset"), "The PC ended the connection" for
`0` and unknown codes, and nothing for `2`, `4` and `5` (whose `Error` it showed
already). The current host logs the headset's reason; its monitors stay for
`--linger` as after any session.

Without a goodbye, implementation-defined details of the current apps:

- Both: no record at all for 5 s ends the session (the other side pings every
  second, so only a dead link or a stopped peer is that quiet). The client then shows
  "No connection to the PC" and looks for the PC again.
- Both: a write blocked for 5 s ends the session.
- Host: TCP keepalive (idle 4 s, interval 2 s, 3 probes) and `TCP_USER_TIMEOUT` 10 s
  also notice a headset that vanished, already while pairing.
- Host: a headset reported as not worn (`Presence{0}`) for 10 s ends the session (with
  goodbye `2`); the host then removes its virtual monitors until the headset connects
  again.
- Host: after a session it keeps the virtual monitors for 30 s (`--linger`).

### Forward compatibility

- **Unknown tags must be skipped.** A reader that gets a tag it does not know reads
  and discards `length` bytes and goes on. Both sides do this, also before the session.
- Unknown values inside a known message (a codec, an enum tag, an `opt` byte) are
  errors, not skipped. A new value of an existing field therefore needs a new message
  or a new protocol version.
- So a new **optional** message needs no new protocol version: older peers skip it.
- Raise `MIN_PROTOCOL` (and `PROTOCOL_VERSION` with it) only for a change an older peer
  cannot just skip: a different layout or meaning of an existing message, a message the
  other side must understand for the session to work, or a change to framing, the
  handshake or pairing. Peers whose ranges no longer overlap then get `Incompatible`
  instead of misreading each other.
- Never reuse a tag number for a different message, and never change the
  [frozen messages](#the-version-handshake-frozen).

## 5. Message reference

Payload notation: fields in order, `name:type`, with the types of
[Encoding](#encoding). Frozen messages are marked as such (their bytes never change).

### Host → headset

| Tag | Name | Payload | Meaning |
|---:|---|---|---|
| 1 | `Welcome` (frozen) | `version:u32, language:str*` | The versions fit. `version`: host `PROTOCOL_VERSION`. `language`: `uk` or `en`. |
| 2 | `StreamStart` | `id:u8, width:u32, height:u32, codec:u8, name:str(max 64)` | A stream begins, or restarts with a new size or codec. `codec`: `0` H.264, `1` H.265 (HEVC); any other value is an error. `width`, `height`: encoded size in pixels. `name`: `Prostir-<id+1>` or `mirror`, informational. |
| 3 | `Frame` | `id:u8, keyframe:bool, pts_us:u64, data:bytes*` | One encoded access unit of stream `id`. See [Frames](#frames). |
| 4 | `StreamEnd` | `id:u8` | The stream is gone (monitor removed, panel closed, mirror stopped). |
| 5 | `PrimaryChanged` | `id:u8` | Monitor `id` is the desktop's primary monitor (KDE: where the Plasma panel is). |
| 6 | `Error` (frozen) | `text:str*` | Something failed; text for the user, in the PC's language. |
| 7 | `MirrorWanted` | `enabled:bool` | Whether the laptop screen should be mirrored into the headset (the host's setting, also the echo of `SetMirror`). |
| 8 | `Order` | `ids:list<u8>(max 16)` | Monitor ids, left to right (rows flattened top to bottom); without the mirror and window panels. |
| 9 | `PairingRequired` | (empty) | The answer to `Open` when the host does not trust this headset: pair first. |
| 10 | `PairMsg` | `msg:bytes[33]` | The host's SPAKE2 message. |
| 11 | `PairResult` | `tag:opt<bytes[32]>` | Whether the codes matched: the host's confirmation tag if they did, none if not. |
| 12 | `Audio` | `pcm:bytes*` | A slice of PC sound: 48 kHz, 2 channels, interleaved signed 16-bit little-endian. |
| 13 | `AudioWanted` | `enabled:bool` | Whether PC sound goes to the headset. |
| 14 | `MicWanted` | `enabled:bool` | Whether the headset microphone should act as the PC's microphone (the host's setting). |
| 15 | `MicActive` | `active:bool` | A PC app is recording from the headset microphone right now (`1`), or stopped (`0`). |
| 16 | `Focus` | `id:opt<u8>` | The stream the mouse is on; none if it is on none of the streamed ones. |
| 17 | `Incompatible` (frozen) | `protocol:u32, app:str*` | The versions do not fit. `protocol`: host `PROTOCOL_VERSION`; `app`: host app version. The host closes the connection after it. |
| 18 | `PcSettings` | `quality:u32, codec:u8, software:bool, sharp:bool, encoders:u8` | How the host encodes. `quality`: bitrate per stream in Mbit/s. `codec`: as in `StreamStart`. `software`: CPU encoder (x264). `sharp`: slower preset for clearer small text. `encoders`: what the PC can use, bits `1` GPU H.264, `2` GPU H.265, `4` CPU H.264. |
| 19 | `Recenter` | (empty) | Turn the monitors to face the user (asked for from the PC tray). |
| 20 | `Notice` | `text:str(max 8192)` | Something done, to show briefly (a saved screenshot, a PC desktop notification). Not an error. |
| 21 | `MouseAt` | `id:u8, x:u16, y:u16` | Answer to `FindMouse`: the mouse is on stream `id` at `x`, `y` across its picture, `0` to `65535` from the top left. |
| 22 | `Abilities` | `bits:u8` | What the PC can do beyond the basics. Bit `1` (`MOVE_MOUSE`): it can move the mouse, so `MouseTo` works. |
| 23 | `SoundFrom` | `id:opt<u8>` | The window playing sound is on stream `id`; none if not known. Sent only while `SetSoundFromScreen` is on, when it changes. |
| 24 | `Opened` | (empty) | The answer to `Open` from a trusted headset: the session started. |
| 25 | `Ping` | `seq:u32` | Sent every second in the session; answer with `Pong{seq}` at once. |
| 26 | `Pong` | `seq:u32` | The answer to the headset's `Ping{seq}`. |
| 27 | `Goodbye` | `reason:u8` | The host ends the session on purpose, for `reason` (see [End of session](#end-of-session)); nothing follows. |

### Headset → host

| Tag | Name | Payload | Meaning |
|---:|---|---|---|
| 101 | `Hello` (frozen) | `version:u32, min:u32, app:str*` | First message of every connection (see [Opening](#opening)). |
| 102 | `RequestKeyframe` | `id:u8` | The decoder of stream `id` lost frames: send a keyframe. |
| 103 | `FrameShown` | `id:u8, pts_us:u64` | The frame with this `pts_us` was handed to the compositor. |
| 104 | `AddMonitor` | `width:u32, height:u32` | Create another virtual monitor of this size in pixels. |
| 105 | `RemoveMonitor` | `id:u8` | Remove monitor `id`. For a window panel id (10 to 12), close the panel and put its window back. |
| 106 | `SetPrimary` | `id:u8` | Make monitor `id` the primary one. Answered with `PrimaryChanged`; an unknown id is ignored. |
| 107 | `SetMirror` | `enabled:bool` | Show or hide the mirror of the laptop screen. Answered with `MirrorWanted`. |
| 108 | `SetOrder` | `ids:list<u8>(max 16)` | New left-to-right order: must contain exactly the ids of the current monitors, otherwise `Error`. Resets the layout to one row. Answered with `Order`. |
| 109 | `PairBegin` | `headset_shows:bool, name:str(max 64)` | Start a pairing attempt. `headset_shows`: `1` the headset shows the code, `0` the host shows it. `name`: label for the host's trust store (the current client sends `Meta Quest`). |
| 110 | `PairMsg` | `msg:bytes[33]` | The headset's SPAKE2 message. |
| 111 | `PairConfirm` | `tag:bytes[32]` | The headset's confirmation tag. |
| 112 | `SetAudio` | `enabled:bool` | Send PC sound to the headset, or keep it on the PC. Answered with `AudioWanted`. |
| 113 | `SetMic` | `enabled:bool` | Use the headset microphone as the PC's microphone. Answered with `MicWanted`. |
| 114 | `Mic` | `pcm:bytes*` | A slice of microphone sound: 48 kHz, 1 channel, signed 16-bit little-endian. |
| 115 | `Presence` | `worn:bool` | Whether the headset is worn. While it is not, the host pauses encoding and sound. |
| 116 | `SetMonitors` | `sizes:list<(width:u32, height:u32)>(max 16), primary:u8` | Switch to this set of monitors, left to right (a profile). `primary` is an index into `sizes`. Monitors whose size already matches are kept. |
| 117 | `SetLayout` | `rows:list<row>(max 16)`, see [Layout](#7-layout) | How the monitors stand around the user. |
| 118 | `SetPcSettings` | `quality:u32, codec:u8, software:bool, sharp:bool` | Change how the PC encodes (same fields as `PcSettings`, without `encoders`). Answered with `PcSettings`, or `Error`. |
| 119 | `Battery` | `percent:u8, charging:bool` | The headset's battery, for the PC tray. |
| 120 | `DetachWindow` | (empty) | Put the PC's active window on a panel of its own. A new stream with an id from 10 to 12 starts, or `Error`. |
| 121 | `WindowHere` | `id:u8` | Move the PC's active window to monitor `id` (a monitor or `100`, the mirror). |
| 122 | `Screenshot` | `id:u8` | Save a picture of monitor or window panel `id` on the PC (of the whole desktop where one output cannot be captured). Answered with `Notice` (where it was saved) or `Error`. |
| 123 | `FindMouse` | (empty) | Where is the mouse? Answered with `MouseAt`, or not at all if the mouse is on none of the streams. |
| 124 | `MouseTo` | `id:u8, x:u16, y:u16` | Put the mouse on stream `id` at `x`, `y` across its picture (`0` to `65535`). Only if `Abilities` has `MOVE_MOUSE`. No answer. |
| 125 | `SetNotifications` | `enabled:bool` | Pass the PC's desktop notifications to the headset as `Notice`. |
| 126 | `SetSoundFromScreen` | `enabled:bool` | Report which stream the sound comes from (`SoundFrom`). |
| 127 | `Open` | (empty) | Start the session, once the headset trusts the host. Answered with `Opened` or `PairingRequired`. |
| 128 | `Ping` | `seq:u32` | Sent every second in the session; answer with `Pong{seq}` at once. |
| 129 | `Pong` | `seq:u32` | The answer to the host's `Ping{seq}`. |
| 130 | `Goodbye` | `reason:u8` | The headset ends the session on purpose, for `reason`; nothing follows. |

Tags 0, 28 to 100 and 131 to 255 are unused.

Host behaviour for monitor requests (implementation-defined): at most 6 monitors; sizes
from 320×240 to 7680×4320; the last monitor cannot be removed; at most 3 window panels.
A request that fails is answered with `Error`.

## 6. Streams, sound and microphone

### Stream ids

| Ids | What | Notes |
|---|---|---|
| `0`, `1`, … | Virtual monitors | The host gives a new monitor the lowest free id. With at most 6 monitors these are `0` to `5`. |
| `10`, `11`, `12` | Window panels (`WINDOW_IDS` = `10..13`) | A small output made for one window, shown as a floating panel. At most three, because the headset has only so many video decoders. |
| `100` | Mirror of the physical display (`MIRROR_ID`) | The laptop panel, streamed as it is. |

### StreamStart

The host sends `StreamStart` before the first `Frame` of a stream on this connection.
It sends it again for the same id when the encoded size changes, when the codec changes
(after `SetPcSettings`), and for every stream at the start of each session. A client
that gets `StreamStart` for an id it already has replaces that stream (new decoder,
new size). `StreamStart` and `StreamEnd` go in the lane of their stream, so they never
overtake a frame of the old configuration that is on its way already. Video frames of
that stream still waiting on the host are discarded when `StreamStart` or `StreamEnd`
is queued, so no other frame of the old configuration follows.

The first `Frame` after a `StreamStart` is a keyframe.

### Frames

- `data` is exactly one access unit as an **Annex B** byte stream (start codes, no
  length prefixes).
- Keyframes carry the parameter sets in-band: SPS and PPS for H.264, VPS, SPS and PPS
  for H.265, in front of the IDR picture.
- `keyframe` is `1` for frames that can be decoded on their own.
- No B-frames: decode order is display order.
- `pts_us` is the presentation timestamp in microseconds from the host's capture clock
  (0 if the encoder gave none). Its starting point is implementation-defined; use it to
  order frames and to refer to them in `FrameShown`, not as wall-clock time.
- Keyframes come mostly on request (at session start, after loss, after
  `RequestKeyframe`); otherwise the encoders put at most 600 frames between two
  keyframes (implementation-defined).

### Feedback from the headset

- **`FrameShown{id, pts_us}`**: the current client sends it for a decoded frame at
  most every 100 ms per stream. The host treats every earlier frame of that stream as
  done too, and uses the acknowledgements only for its latency figures in the log.
  They are not flow control: a host must not wait for them.
- **`RequestKeyframe{id}`**: the client sends it when it has had to drop frames (its
  queue for that decoder, 32 frames in the current client, was full) or its decoder is
  waiting for a keyframe; it then drops non-key frames of that stream until a keyframe
  arrives. The current client asks at most every 300 ms per stream. The host asks its
  encoder for a keyframe.

### When frames are late

The host picks every packet it sends (implementation-defined, `host/src/outbox.rs`),
in strict priority:

1. control messages (everything except `Audio`, `Frame`, `StreamStart` and
   `StreamEnd`), in order, in lane `0`; never dropped, but over 4 MiB of them waiting
   means the headset reads nothing, and the session ends;
2. sound (`Audio`), in lane `1`, at most 8 slices waiting; the oldest is dropped when a
   newer one arrives;
3. video, each stream in a lane of its own (`2 + id`), the streams taking turns a
   packet each.

A message it started is always finished; one of a higher class goes between its
packets. So sound waits behind at most one packet (16 KiB) of a big keyframe, and one
stream's keyframe does not hold up the others.

A non-key video frame that has waited more than 120 ms is not sent. The host then
drops every later frame of that stream until the next keyframe (a decoder cannot use
them without the lost one) and asks its encoder for a keyframe, at most every 300 ms.
Keyframes are never dropped for being late, and a keyframe queued drops the frames of
its stream still waiting before it (it replaces them). If more than 16 MiB of video is
waiting, new non-key frames are refused the same way. Other streams are not affected.

The host also paces the connection (`SO_MAX_PACING_RATE`): it sends at most 1.5 times
what its encoders make plus 4 Mbit/s, and never less than 20 Mbit/s, so a keyframe
does not land in the Wi-Fi access point's queue all at once with sound stuck behind
it, where nothing can overtake it any more. It keeps little unsent data in the kernel
(`TCP_NOTSENT_LOWAT` 32 KiB) for the same reason.

These values are implementation-defined; the rule a client can rely on is that
**after a gap in a stream, the next frame it gets for that stream is a keyframe**.

### Sound and microphone

| | Direction | Format | Message |
|---|---|---|---|
| PC sound | host → headset | 48 kHz (`AUDIO_RATE`), 2 channels (`AUDIO_CHANNELS`), interleaved signed 16-bit little-endian | `Audio` |
| Microphone | headset → host | 48 kHz, 1 channel (`MIC_CHANNELS`), signed 16-bit little-endian | `Mic` |

There is no header or timestamp: each message is a whole number of sample frames, to
be played in arrival order. Slices are 10 ms in the current apps (1920 bytes of PC
sound, 960 bytes of microphone), but the size is implementation-defined and a receiver
must accept any length.

The current client sends control messages first, then microphone sound and
`FrameShown` in a lane of their own; while the network is stuck it drops those beyond
about 2.5 s waiting, and never a control message.

PC sound flows while the headset is worn and `AudioWanted` is on. The microphone:
`SetMic{1}` tells the host to offer the headset microphone as a PC input; the host sends
`MicActive{1}` while a PC app records from it, and the headset then sends `Mic` slices
until `MicActive{0}`, but only if its own microphone switch is on. The host cannot turn
that switch on. The current client ignores `MicWanted`.

## 7. Layout

`SetLayout` tells the host how the monitors stand around the user, so the PC desktop
can be arranged the same way and the mouse crosses from monitor to monitor where the
eyes expect it.

### Encoding

```
SetLayout: rows:list<row>(max 16)
row:       entries:list<entry>(max 16)
entry:     id:u8, center:opt<i32>, middle:opt<i32>
```

Rows go from top to bottom, entries in a row from left to right. An empty list means no
rows. A `SetLayout` may hold at most 16 rows and at most 16 monitors in all its rows
together, and lists each monitor once; a reader rejects any other as invalid, and the
connection ends.

- `id`: a monitor id, or `100` for the mirror if the headset placed it. Window panels
  are not part of the layout.
- `center`: where the monitor's centre is seen horizontally; none: "just right of the
  previous monitor in the row".
- `middle`: where the monitor's centre is seen vertically, **downwards**; none: "not
  known, centred in the row".

### Units

`center` and `middle` are in pixels of one scale shared by all monitors. Only
differences between values matter; the origin is arbitrary. The current client
computes them from the angles the monitors take up as seen from where the layout faces:
the horizontal angle (yaw) and the elevation, times pixels per radian, which is the
average over the monitors of their pixel width divided by their angular width. Values
are rounded to multiples of 8, so a hand's tremor does not rearrange the desktop. This
method is implementation-defined; what the host relies on is that the values are
comparable between monitors and to the monitors' own pixel sizes.

### What the host does with it

(Implementation-defined; `host/src/displays.rs` and `host/src/layout.rs`.)

1. Clamps every known `center` and `middle` to ±100000.
2. Keeps the rows, drops ids that are neither a current monitor nor the active mirror,
   and appends monitors missing from the rows to the end of the last row, with no
   positions.
3. Sets the monitor order to the ids of all rows, top to bottom and left to right,
   without the mirror, and sends it back as `Order`.
4. Arranges the desktop, in logical pixels (pixel size divided by the output's scale):
   - If every entry has a known `center` and `middle`, the desktop follows what is seen:
     starting from the primary monitor, each monitor joins the nearest one already
     placed, on the side where they meet in the headset, edge to edge, offset along that
     edge as seen (lined up when within 24 px), sharing at least 200 px of the edge (or
     the whole edge if shorter) so the mouse can cross, and overlapping nothing.
   - Otherwise it arranges by rows: the row with the primary monitor side by side
     (vertically by `middle` if all are known, else centred), and every monitor of
     another row above or below the one it overlaps most, keeping the horizontal
     offset seen in the headset.
   - The mirror goes under the primary monitor unless the headset placed it; window
     panels go in a column to the right of everything.
5. Saves the layout for the next session.

`SetOrder` and `SetMonitors` replace the layout with one row in the given order, with
no positions.
