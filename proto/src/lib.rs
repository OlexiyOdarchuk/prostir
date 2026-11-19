//! Wire protocol between the PC host and the Quest client.
//!
//! The client connects over TCP (USB via `adb reverse`, or Wi-Fi after discovering the
//! host through a UDP broadcast beacon), and both sides run an encrypted handshake
//! ([`secure`]). After it every message is framed as
//! `[tag: u8][len: u32 LE][payload: len bytes]`. A payload is the message's fields
//! encoded as [`wire`] describes, and must be read whole: bytes left over are an error.
//! A message with a tag a reader does not know is skipped, so a newer peer can add
//! messages without a new protocol version.
//!
//! # The version handshake (frozen)
//!
//! The headset's first message is [`ClientMsg::Hello`], before anything else (pairing
//! too). The PC answers with [`HostMsg::Welcome`] when the versions fit, or with
//! [`HostMsg::Error`] and [`HostMsg::Incompatible`] when they do not, and then closes
//! the connection. These four messages keep the bytes they had in protocol 1 in every
//! version, so any two versions can tell the user which side to update: the fixed
//! fields, then a UTF-8 text running to the end of the payload. They must never
//! change; what a new version needs goes into other messages.
//!
//! Only the media messages ([`HostMsg::Frame`], [`HostMsg::Audio`], [`ClientMsg::Mic`])
//! end in bytes running to the end of the payload too. They are read and written by
//! hand, so their data is never copied on the way.

// Everything here parses data from the network: no unsafe code at all.
#![forbid(unsafe_code)]

pub mod secure;
pub mod stats;
pub mod wire;

use std::io::{self, Read, Write};

use wire::{Reader, Wire, WireError, wire_enum, wire_struct};

pub const TCP_PORT: u16 = 47830;
pub const DISCOVERY_PORT: u16 = 47831;
/// Newest protocol this build speaks. Messages added later are skipped by older peers,
/// so a new optional message does not need a new version.
pub const PROTOCOL_VERSION: u32 = 2;
/// Oldest peer protocol this build can still talk to. Raise it only for a change an
/// older peer cannot just skip.
pub const MIN_PROTOCOL: u32 = 2;

/// Whether a peer that speaks `version` and still understands down to `min` can talk
/// to this build.
pub fn compatible(version: u32, min: u32) -> bool {
    version >= MIN_PROTOCOL && min <= PROTOCOL_VERSION
}
/// PC sound sent to the headset: interleaved signed 16-bit little-endian stereo.
pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u32 = 2;
/// Headset microphone sent to the PC: interleaved signed 16-bit little-endian mono.
pub const MIC_CHANNELS: u32 = 1;

/// What the PC can encode with ([`HostMsg::PcSettings`]).
pub mod encoders {
    pub const GPU_H264: u8 = 1;
    pub const GPU_H265: u8 = 2;
    pub const CPU_H264: u8 = 4;
}

/// What the PC can do beyond the basics ([`HostMsg::Abilities`]).
pub mod abilities {
    /// Move the mouse (to where the user looks).
    pub const MOVE_MOUSE: u8 = 1;
}

/// Most virtual monitors a PC makes: more streams than this overwhelm the headset's
/// hardware decoders.
pub const MAX_MONITORS: usize = 6;

/// Stream ids of window panels: small outputs made for one window each, shown as
/// floating panels (at most three: the headset has only so many video decoders).
pub const WINDOW_IDS: std::ops::Range<u8> = 10..13;

/// Whether `id` is a window panel (see [`WINDOW_IDS`]).
pub fn is_window_panel(id: u8) -> bool {
    WINDOW_IDS.contains(&id)
}

/// Stream id of the mirrored physical display (laptop panel); Quest monitors use 0..
pub const MIRROR_ID: u8 = 100;
/// Prefix of the UDP discovery beacon: `MAGIC ++ tcp_port (u16 LE) ++ hostname (utf-8)`.
pub const BEACON_MAGIC: &[u8; 8] = b"PROSTIR1";

/// Largest message from the PC (a video frame can be several megabytes).
const MAX_HOST_PAYLOAD: u32 = 64 * 1024 * 1024;
/// Largest message from the PC before the session is open ([`HostMsg::read_setup`]):
/// the PC is not trusted yet, and nothing it sends then is big.
const MAX_SETUP_PAYLOAD: u32 = 64 * 1024;
/// Largest message from the headset: the biggest real one is a slice of microphone
/// sound (under 1 KiB). A peer is not trusted yet when its first messages arrive, so
/// this keeps what it can make the PC allocate small.
const MAX_CLIENT_PAYLOAD: u32 = 64 * 1024;
/// Most rows, or monitors in all rows, a layout may list (there are at most six
/// monitors, a laptop screen and three window panels). Also the most monitors in an
/// order or a set of monitors.
const MAX_LAYOUT_ENTRIES: usize = 16;
/// Longest name of a stream or a headset, in bytes.
const MAX_NAME: usize = 64;
/// Longest notice, in bytes (a desktop notification, or a path on the PC).
const MAX_TEXT: usize = 8 * 1024;

wire_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Codec {
        0 => H264,
        1 => H265,
    }
}

impl Codec {
    pub fn mime(self) -> &'static str {
        match self {
            Codec::H264 => "video/avc",
            Codec::H265 => "video/hevc",
        }
    }
}

wire_struct! {
    /// How the PC encodes, changed from the headset menu ([`HostMsg::PcSettings`],
    /// [`ClientMsg::SetPcSettings`]).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PcSettings {
        /// Bitrate per monitor, Mbit/s.
        pub quality: u32,
        pub codec: Codec,
        /// Encode on the CPU (x264) instead of the graphics card.
        pub software: bool,
        /// Slower encoder preset for clearer small text.
        pub sharp: bool,
    }
}

wire_struct! {
    /// One row of monitors in [`ClientMsg::SetLayout`].
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct LayoutRow {
        /// Left to right.
        #[wire(max = MAX_LAYOUT_ENTRIES)]
        pub entries: Vec<LayoutEntry>,
    }
}

impl LayoutRow {
    /// The monitor ids, left to right.
    pub fn ids(&self) -> impl Iterator<Item = u8> + '_ {
        self.entries.iter().map(|e| e.id)
    }
}

wire_struct! {
    /// A monitor in a [`LayoutRow`] and where its centre is seen, in pixels of one
    /// scale shared by all monitors. The PC lines monitors of other rows up with
    /// these, so one hung above another is above it on the desktop too.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct LayoutEntry {
        pub id: u8,
        /// Across, left to right (`None`: just right of the previous one).
        pub center: Option<i32>,
        /// Down (`None`: not known, centred in the row). A portrait monitor whose top
        /// lines up with a landscape one's stands that way on the desktop too.
        pub middle: Option<i32>,
    }
}

wire_enum! {
    message
    /// From the PC to the headset.
    #[derive(Debug, Clone, PartialEq)]
    pub enum HostMsg {
        /// The versions fit: the answer to [`ClientMsg::Hello`]. Frozen (see the crate
        /// root).
        1 => Welcome {
            /// The PC's [`PROTOCOL_VERSION`].
            version: u32,
            /// The PC's interface language (`uk`, `en`), which the headset follows by
            /// default.
            #[wire(rest)]
            language: String,
        },
        /// The session started (the answer to [`ClientMsg::Open`] from a headset the
        /// PC trusts). The PC's state follows.
        24 => Opened,
        /// A virtual monitor appeared. `width`/`height` are in pixels.
        2 => StreamStart {
            id: u8,
            width: u32,
            height: u32,
            codec: Codec,
            #[wire(max = MAX_NAME)]
            name: String,
        },
        /// One encoded access unit (Annex-B byte stream, parameter sets in-band on
        /// keyframes).
        3 => Frame {
            id: u8,
            keyframe: bool,
            pts_us: u64,
            #[wire(rest)]
            data: Vec<u8>,
        },
        4 => StreamEnd { id: u8 },
        /// This monitor is KDE's primary one (the Plasma panel lives there).
        5 => PrimaryChanged { id: u8 },
        /// Left-to-right order of the Quest monitors (as in KDE's layout).
        8 => Order {
            #[wire(max = MAX_LAYOUT_ENTRIES)]
            ids: Vec<u8>,
        },
        /// Whether the physical display should be mirrored into the headset.
        7 => MirrorWanted { enabled: bool },
        /// Something failed; shown in the headset menu. Frozen (see the crate root):
        /// it also tells headsets older than [`HostMsg::Incompatible`] what to update.
        6 => Error {
            #[wire(rest)]
            text: String,
        },
        /// A slice of PC sound (see [`AUDIO_RATE`]).
        12 => Audio {
            #[wire(rest)]
            pcm: Vec<u8>,
        },
        /// Whether PC sound goes to the headset.
        13 => AudioWanted { enabled: bool },
        /// Whether the headset microphone should be the PC's microphone.
        14 => MicWanted { enabled: bool },
        /// A PC app is recording from the headset microphone right now: capture it.
        15 => MicActive { active: bool },
        /// The headset speaks another protocol version: one side has to be updated.
        /// Sent instead of [`HostMsg::Welcome`], after an [`HostMsg::Error`] saying the
        /// same; the PC closes the connection after it. Frozen (see the crate root).
        17 => Incompatible {
            /// The PC's protocol version.
            protocol: u32,
            /// The PC app's version, e.g. `0.2.0`.
            #[wire(rest)]
            app: String,
        },
        /// Turn the monitors to face the user (asked for from the PC's tray).
        19 => Recenter,
        /// Something done, to show briefly (not an error): "Saved to ~/Pictures/…".
        20 => Notice {
            #[wire(max = MAX_TEXT)]
            text: String,
        },
        /// Where the mouse is: monitor `id`, `x`/`y` across its image (0..=65535 from
        /// the top left). The answer to [`ClientMsg::FindMouse`].
        21 => MouseAt { id: u8, x: u16, y: u16 },
        /// What this PC can do (bits of [`abilities`]).
        22 => Abilities { bits: u8 },
        /// The sound comes from the window on monitor `id` (`None`: not known).
        23 => SoundFrom { id: Option<u8> },
        /// The PC's encoding settings and what it can encode with (bits of
        /// [`encoders`]).
        18 => PcSettings { settings: PcSettings, encoders: u8 },
        /// The monitor the mouse is on (`None`: none of the streamed ones).
        16 => Focus { id: Option<u8> },
        /// The PC does not know this headset yet: pair first (the answer to
        /// [`ClientMsg::Open`]).
        9 => PairingRequired,
        /// The PC's SPAKE2 message (see [`secure::Pairing`]).
        10 => PairMsg { msg: [u8; secure::PAIR_MSG_LEN] },
        /// Whether the codes matched: the PC's confirmation tag if they did.
        11 => PairResult { tag: Option<[u8; secure::CONFIRM_LEN]> },
    }
}

wire_enum! {
    message
    /// From the headset to the PC.
    #[derive(Debug, Clone, PartialEq)]
    pub enum ClientMsg {
        /// The first message of every connection. Frozen (see the crate root).
        101 => Hello {
            /// Newest protocol the headset speaks ([`PROTOCOL_VERSION`]).
            version: u32,
            /// Oldest protocol it still understands ([`MIN_PROTOCOL`]).
            min: u32,
            /// The headset app's version (e.g. `0.2.0`), to say what to update.
            #[wire(rest)]
            app: String,
        },
        /// Start the session, once the headset trusts the PC. Answered with
        /// [`HostMsg::Opened`], or [`HostMsg::PairingRequired`].
        127 => Open,
        102 => RequestKeyframe { id: u8 },
        /// The decoder handed the frame with this `pts_us` to the compositor.
        103 => FrameShown { id: u8, pts_us: u64 },
        /// Create another virtual monitor of this size in pixels.
        104 => AddMonitor { width: u32, height: u32 },
        105 => RemoveMonitor { id: u8 },
        /// New left-to-right order of the Quest monitors.
        108 => SetOrder {
            #[wire(max = MAX_LAYOUT_ENTRIES)]
            ids: Vec<u8>,
        },
        /// Show or hide the mirror of the physical display.
        107 => SetMirror { enabled: bool },
        /// Make this monitor KDE's primary one.
        106 => SetPrimary { id: u8 },
        /// Send PC sound to the headset (or keep it on the PC).
        112 => SetAudio { enabled: bool },
        /// Use the headset microphone as the PC's microphone.
        113 => SetMic { enabled: bool },
        /// A slice of headset microphone sound (see [`MIC_CHANNELS`]).
        114 => Mic {
            #[wire(rest)]
            pcm: Vec<u8>,
        },
        /// Switch to this set of monitors, left to right (a profile). Monitors whose
        /// size already matches are kept, so their windows stay where they are.
        116 => SetMonitors {
            /// Width and height in pixels.
            #[wire(max = MAX_LAYOUT_ENTRIES)]
            sizes: Vec<(u32, u32)>,
            /// Index into `sizes`.
            primary: u8,
        },
        /// How the monitors stand around the user, for the desktop's arrangement: rows
        /// from top to bottom, with where each monitor is seen (see [`LayoutRow`]); the
        /// PC lays its outputs out to match. At most 16 monitors in all rows together,
        /// each of them once.
        117 => SetLayout {
            #[wire(max = MAX_LAYOUT_ENTRIES)]
            rows: Vec<LayoutRow>,
        },
        /// Whether the headset is being worn; streaming pauses while it is not.
        115 => Presence { worn: bool },
        /// Change how the PC encodes (from the headset menu).
        118 => SetPcSettings { settings: PcSettings },
        /// The headset's battery, for the PC's tray.
        119 => Battery { percent: u8, charging: bool },
        /// Put the PC's active window on a panel of its own (a small output its size).
        120 => DetachWindow,
        /// Move the PC's active window to monitor `id` (the one the user looks at).
        121 => WindowHere { id: u8 },
        /// Save a picture of monitor `id` on the PC.
        122 => Screenshot { id: u8 },
        /// Where is the mouse? (answered with [`HostMsg::MouseAt`])
        123 => FindMouse,
        /// Pass the PC's desktop notifications to the headset (as [`HostMsg::Notice`]).
        125 => SetNotifications { enabled: bool },
        /// Say which monitor the sound comes from ([`HostMsg::SoundFrom`]).
        126 => SetSoundFromScreen { enabled: bool },
        /// Put the mouse on monitor `id`, at `x`/`y` across its image (0..=65535).
        124 => MouseTo { id: u8, x: u16, y: u16 },
        /// Start pairing. `headset_shows`: the headset displays the code and the PC
        /// asks for it; otherwise the PC displays it and the headset asks.
        109 => PairBegin {
            headset_shows: bool,
            #[wire(max = MAX_NAME)]
            name: String,
        },
        /// The headset's SPAKE2 message.
        110 => PairMsg { msg: [u8; secure::PAIR_MSG_LEN] },
        /// Proof that the headset derived the same pairing key.
        111 => PairConfirm { tag: [u8; secure::CONFIRM_LEN] },
    }
}

/// Tags the code below needs by name (the tests check them against the
/// declarations): the media messages, read and written by hand, and the hello.
mod tag {
    pub const FRAME: u8 = 3;
    pub const AUDIO: u8 = 12;
    pub const MIC: u8 = 114;
    pub const HELLO: u8 = 101;
}

/// [`HostMsg::Frame`] before its data: id, keyframe, pts_us.
const FRAME_HEAD: usize = 1 + 1 + 8;

/// A message this side cannot send: a field over its limit, or too large as a whole.
/// The sender's mistake, so it ends this connection and nothing else.
fn unsendable(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, e)
}

/// The 5 bytes before a payload of `len` bytes.
fn header(tag: u8, len: usize) -> io::Result<[u8; 5]> {
    let len = u32::try_from(len).map_err(|_| unsendable("message too large"))?;
    let mut header = [0u8; 5];
    header[0] = tag;
    header[1..].copy_from_slice(&len.to_le_bytes());
    Ok(header)
}

/// A media message: its header, then its parts as they are (no copy of the data).
fn write_media(w: &mut impl Write, tag: u8, parts: &[&[u8]]) -> io::Result<()> {
    w.write_all(&header(tag, parts.iter().map(|p| p.len()).sum())?)?;
    for p in parts {
        w.write_all(p)?;
    }
    Ok(())
}

/// Any other message, header and payload in one buffer and one write.
fn write_fields(
    w: &mut impl Write,
    tag: u8,
    put: impl FnOnce(&mut Vec<u8>) -> Result<(), WireError>,
) -> io::Result<()> {
    let mut out = vec![0u8; 5];
    put(&mut out).map_err(unsendable)?;
    let head = header(tag, out.len() - 5)?;
    out[..5].copy_from_slice(&head);
    w.write_all(&out)
}

/// The tag and payload length of the next message.
fn read_header(r: &mut impl Read, max: u32) -> io::Result<(u8, usize)> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    let [tag, len @ ..] = header;
    let len = u32::from_le_bytes(len);
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    Ok((tag, len as usize))
}

fn read_bytes(r: &mut impl Read, len: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Reads past a payload without keeping it.
fn skip(r: &mut impl Read, len: usize) -> io::Result<()> {
    let skipped = io::copy(&mut r.by_ref().take(len as u64), &mut io::sink())?;
    if skipped < len as u64 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

impl HostMsg {
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            HostMsg::Frame {
                id,
                keyframe,
                pts_us,
                data,
            } => {
                let mut head = [0u8; FRAME_HEAD];
                head[0] = *id;
                head[1] = u8::from(*keyframe);
                head[2..].copy_from_slice(&pts_us.to_le_bytes());
                write_media(w, tag::FRAME, &[&head, data])
            }
            HostMsg::Audio { pcm } => write_media(w, tag::AUDIO, &[pcm]),
            _ => write_fields(w, self.wire_tag(), |out| self.put_fields(out)),
        }
    }

    /// The next message this build knows; messages from a newer PC are skipped.
    pub fn read(r: &mut impl Read) -> io::Result<Self> {
        Self::read_within(r, MAX_HOST_PAYLOAD)
    }

    /// As [`HostMsg::read`], until [`HostMsg::Opened`]: the PC may not be trusted yet,
    /// so a message over 64 KiB is an error.
    pub fn read_setup(r: &mut impl Read) -> io::Result<Self> {
        Self::read_within(r, MAX_SETUP_PAYLOAD)
    }

    fn read_within(r: &mut impl Read, max: u32) -> io::Result<Self> {
        loop {
            let (tag, len) = read_header(r, max)?;
            match tag {
                tag::FRAME => {
                    if len < FRAME_HEAD {
                        return Err(WireError::Truncated.into());
                    }
                    let mut head = [0u8; FRAME_HEAD];
                    r.read_exact(&mut head)?;
                    let mut h = Reader::new(&head);
                    let (id, keyframe, pts_us) =
                        (u8::get(&mut h)?, bool::get(&mut h)?, u64::get(&mut h)?);
                    let data = read_bytes(r, len - FRAME_HEAD)?;
                    return Ok(HostMsg::Frame {
                        id,
                        keyframe,
                        pts_us,
                        data,
                    });
                }
                tag::AUDIO => {
                    return Ok(HostMsg::Audio {
                        pcm: read_bytes(r, len)?,
                    });
                }
                tag if Self::TAGS.contains(&tag) => {
                    return Ok(Self::decode(tag, &read_bytes(r, len)?)?);
                }
                _ => skip(r, len)?,
            }
        }
    }

    fn decode(tag: u8, payload: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(payload);
        let msg = Self::get_fields(tag, &mut r)?.ok_or(WireError::UnknownTag {
            what: "HostMsg",
            tag,
        })?;
        r.finish()?;
        Ok(msg)
    }
}

impl ClientMsg {
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            ClientMsg::Mic { pcm } => write_media(w, tag::MIC, &[pcm]),
            _ => write_fields(w, self.wire_tag(), |out| self.put_fields(out)),
        }
    }

    /// The first message of a connection, which every headset since protocol 2 makes
    /// [`ClientMsg::Hello`]. Any other gives `None`, skipped unread: a headset from
    /// before protocol 2 may ask to pair before saying hello, in messages this build
    /// cannot read.
    pub fn read_first(r: &mut impl Read) -> io::Result<Option<Self>> {
        let (tag, len) = read_header(r, MAX_CLIENT_PAYLOAD)?;
        if tag != tag::HELLO {
            skip(r, len)?;
            return Ok(None);
        }
        Ok(Some(Self::decode(tag, &read_bytes(r, len)?)?))
    }

    /// The next message this build knows; messages from a newer headset are skipped.
    pub fn read(r: &mut impl Read) -> io::Result<Self> {
        loop {
            let (tag, len) = read_header(r, MAX_CLIENT_PAYLOAD)?;
            match tag {
                tag::MIC => {
                    return Ok(ClientMsg::Mic {
                        pcm: read_bytes(r, len)?,
                    });
                }
                tag if Self::TAGS.contains(&tag) => {
                    return Ok(Self::decode(tag, &read_bytes(r, len)?)?);
                }
                _ => skip(r, len)?,
            }
        }
    }

    fn decode(tag: u8, payload: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(payload);
        let msg = Self::get_fields(tag, &mut r)?.ok_or(WireError::UnknownTag {
            what: "ClientMsg",
            tag,
        })?;
        r.finish()?;
        if let ClientMsg::SetLayout { rows } = &msg {
            // Each monitor once, so the order the PC makes from it fits in a message.
            let len = rows.iter().map(|r| r.entries.len()).sum();
            if len > MAX_LAYOUT_ENTRIES {
                return Err(WireError::TooLong {
                    len,
                    max: MAX_LAYOUT_ENTRIES,
                });
            }
            let mut seen = Vec::with_capacity(len);
            for id in rows.iter().flat_map(LayoutRow::ids) {
                if seen.contains(&id) {
                    return Err(WireError::Repeated(id));
                }
                seen.push(id);
            }
        }
        Ok(msg)
    }
}

/// Builds a discovery beacon payload.
pub fn encode_beacon(tcp_port: u16, hostname: &str) -> Vec<u8> {
    let mut v = BEACON_MAGIC.to_vec();
    v.extend_from_slice(&tcp_port.to_le_bytes());
    v.extend_from_slice(hostname.as_bytes());
    v
}

/// Parses a discovery beacon, returning `(tcp_port, hostname)`.
pub fn decode_beacon(buf: &[u8]) -> Option<(u16, String)> {
    let rest = buf.strip_prefix(BEACON_MAGIC.as_slice())?;
    let port = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?);
    Some((port, String::from_utf8_lossy(&rest[2..]).into_owned()))
}

#[cfg(test)]
mod tests;
