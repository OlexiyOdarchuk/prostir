//! Wire protocol between the PC host and the Quest client.
//!
//! Every message is framed as `[tag: u8][len: u32 LE][payload: len bytes]`.
//! The client connects over TCP (USB via `adb reverse`, or Wi-Fi after
//! discovering the host through a UDP broadcast beacon).

// Everything here parses data from the network: no unsafe code at all.
#![forbid(unsafe_code)]

pub mod secure;

use std::io::{self, Read, Write};

pub const TCP_PORT: u16 = 47830;
pub const DISCOVERY_PORT: u16 = 47831;
/// Newest protocol this build speaks. Messages added later are skipped by older peers
/// (see [`HostMsg::Unknown`]), so a new optional message does not need a new version.
pub const PROTOCOL_VERSION: u32 = 1;
/// Oldest peer protocol this build can still talk to. Raise it only for a change an
/// older peer cannot just skip.
pub const MIN_PROTOCOL: u32 = 1;

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
/// One row of monitors in [`ClientMsg::SetLayout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutRow {
    /// Monitor ids, left to right.
    pub ids: Vec<u8>,
    /// Where each monitor's centre is seen, left to right, in pixels of one scale
    /// shared by all monitors ([`NEXT`]: just right of the previous one). The PC lines
    /// monitors of other rows up with these, so one hung above another is above it on
    /// the desktop too.
    pub centers: Vec<i32>,
}

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

impl PcSettings {
    fn bytes(&self) -> [u8; 7] {
        let q = self.quality.to_le_bytes();
        [
            q[0],
            q[1],
            q[2],
            q[3],
            self.codec as u8,
            self.software as u8,
            self.sharp as u8,
        ]
    }

    fn read(c: &mut Cursor) -> io::Result<Self> {
        Ok(Self {
            quality: c.u32()?,
            codec: Codec::from_u8(c.u8()?).unwrap_or(Codec::H265),
            software: c.u8()? != 0,
            sharp: c.u8()? != 0,
        })
    }
}

/// What the PC can encode with ([`HostMsg::PcSettings`]).
pub mod encoders {
    pub const GPU_H264: u8 = 1;
    pub const GPU_H265: u8 = 2;
    pub const CPU_H264: u8 = 4;
}

/// In [`LayoutRow::centers`]: no position, right after the previous monitor.
pub const NEXT: i32 = i32::MIN;

/// [`HostMsg::Focus`] when the mouse is on none of the streamed displays.
pub const NO_FOCUS: u8 = 255;
/// Stream id of the mirrored physical display (laptop panel); Quest monitors use 0..
pub const MIRROR_ID: u8 = 100;
/// Prefix of the UDP discovery beacon: `MAGIC ++ tcp_port (u16 LE) ++ hostname (utf-8)`.
pub const BEACON_MAGIC: &[u8; 8] = b"PROSTIR1";

const MAX_PAYLOAD: u32 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264 = 0,
    H265 = 1,
}

impl Codec {
    pub fn mime(self) -> &'static str {
        match self {
            Codec::H264 => "video/avc",
            Codec::H265 => "video/hevc",
        }
    }
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Codec::H264),
            1 => Some(Codec::H265),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HostMsg {
    /// Sent once after the handshake.
    Welcome {
        version: u32,
        /// The PC's interface language (`uk`, `en`), which the headset follows by default.
        language: String,
    },
    /// A virtual monitor appeared. `width`/`height` are in pixels.
    StreamStart {
        id: u8,
        width: u32,
        height: u32,
        codec: Codec,
        name: String,
    },
    /// One encoded access unit (Annex-B byte stream, parameter sets in-band on keyframes).
    Frame {
        id: u8,
        pts_us: u64,
        keyframe: bool,
        data: Vec<u8>,
    },
    StreamEnd {
        id: u8,
    },
    /// This monitor is KDE's primary one (the Plasma panel lives there).
    PrimaryChanged {
        id: u8,
    },
    /// Left-to-right order of the Quest monitors (as in KDE's layout).
    Order {
        ids: Vec<u8>,
    },
    /// Whether the physical display should be mirrored into the headset.
    MirrorWanted {
        enabled: bool,
    },
    /// Something the user asked for failed; shown in the headset menu.
    Error {
        text: String,
    },
    /// A slice of PC sound (see [`AUDIO_RATE`]).
    Audio {
        pcm: Vec<u8>,
    },
    /// Whether PC sound goes to the headset.
    AudioWanted {
        enabled: bool,
    },
    /// Whether the headset microphone should be the PC's microphone.
    MicWanted {
        enabled: bool,
    },
    /// A PC app is recording from the headset microphone right now: capture it.
    MicActive {
        active: bool,
    },
    /// The headset speaks another protocol version: one side has to be updated. Sent
    /// instead of [`HostMsg::Welcome`]; the PC closes the connection after it.
    Incompatible {
        /// The PC's protocol version.
        protocol: u32,
        /// The PC app's version, e.g. `0.2.0`.
        app: String,
    },
    /// Turn the monitors to face the user (asked for from the PC's tray).
    Recenter,
    /// The PC's encoding settings and what it can encode with (bits of [`encoders`]).
    PcSettings {
        settings: PcSettings,
        encoders: u8,
    },
    /// The monitor the mouse is on ([`NO_FOCUS`] if none of the streamed ones).
    Focus {
        id: u8,
    },
    /// The PC does not know this headset yet: pair first.
    PairingRequired,
    /// The PC's SPAKE2 message (see [`secure::Pairing`]).
    PairMsg {
        msg: Vec<u8>,
    },
    /// Whether the codes matched; on success `tag` proves it from the PC's side.
    PairResult {
        ok: bool,
        tag: Vec<u8>,
    },
    /// A message from a newer PC that this build does not know; skipped.
    Unknown {
        tag: u8,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientMsg {
    Hello {
        /// Newest protocol the headset speaks ([`PROTOCOL_VERSION`]).
        version: u32,
        /// Oldest protocol it still understands ([`MIN_PROTOCOL`]).
        min: u32,
        /// The headset app's version (e.g. `0.2.0`), to say what to update.
        app: String,
    },
    /// A message from a newer headset that this build does not know; skipped.
    Unknown {
        tag: u8,
    },
    RequestKeyframe {
        id: u8,
    },
    /// The decoder handed the frame with this `pts_us` to the compositor.
    FrameShown {
        id: u8,
        pts_us: u64,
    },
    /// Create another virtual monitor of this size in pixels.
    AddMonitor {
        width: u32,
        height: u32,
    },
    RemoveMonitor {
        id: u8,
    },
    /// New left-to-right order of the Quest monitors.
    SetOrder {
        ids: Vec<u8>,
    },
    /// Show or hide the mirror of the physical display.
    SetMirror {
        enabled: bool,
    },
    /// Make this monitor KDE's primary one.
    SetPrimary {
        id: u8,
    },
    /// Send PC sound to the headset (or keep it on the PC).
    SetAudio {
        enabled: bool,
    },
    /// Use the headset microphone as the PC's microphone.
    SetMic {
        enabled: bool,
    },
    /// A slice of headset microphone sound (see [`MIC_CHANNELS`]).
    Mic {
        pcm: Vec<u8>,
    },
    /// Switch to this set of monitors, left to right (a profile). Monitors whose size
    /// already matches are kept, so their windows stay where they are.
    SetMonitors {
        sizes: Vec<(u32, u32)>,
        /// Index into `sizes`.
        primary: u8,
    },
    /// How the monitors stand around the user, for KDE's arrangement: rows from top to
    /// bottom, each left to right, with the left edge of the row in desktop pixels
    /// (relative; only differences between rows matter).
    SetLayout {
        rows: Vec<LayoutRow>,
    },
    /// Whether the headset is being worn; streaming pauses while it is not.
    Presence {
        worn: bool,
    },
    /// Change how the PC encodes (from the headset menu).
    SetPcSettings {
        settings: PcSettings,
    },
    /// The headset's battery, for the PC's tray.
    Battery {
        percent: u8,
        charging: bool,
    },
    /// Start pairing. `headset_shows`: the headset displays the code and the PC asks
    /// for it; otherwise the PC displays it and the headset asks.
    PairBegin {
        headset_shows: bool,
        name: String,
    },
    /// The headset's SPAKE2 message.
    PairMsg {
        msg: Vec<u8>,
    },
    /// Proof that the headset derived the same pairing key.
    PairConfirm {
        tag: Vec<u8>,
    },
}

mod tag {
    pub const WELCOME: u8 = 1;
    pub const STREAM_START: u8 = 2;
    pub const FRAME: u8 = 3;
    pub const STREAM_END: u8 = 4;
    pub const PRIMARY_CHANGED: u8 = 5;
    pub const ERROR: u8 = 6;
    pub const MIRROR_WANTED: u8 = 7;
    pub const ORDER: u8 = 8;
    pub const PAIRING_REQUIRED: u8 = 9;
    pub const PAIR_MSG_HOST: u8 = 10;
    pub const PAIR_RESULT: u8 = 11;
    pub const AUDIO: u8 = 12;
    pub const AUDIO_WANTED: u8 = 13;
    pub const MIC_WANTED: u8 = 14;
    pub const MIC_ACTIVE: u8 = 15;
    pub const FOCUS: u8 = 16;
    pub const INCOMPATIBLE: u8 = 17;
    pub const PC_SETTINGS: u8 = 18;
    pub const RECENTER: u8 = 19;
    pub const HELLO: u8 = 101;
    pub const REQUEST_KEYFRAME: u8 = 102;
    pub const FRAME_SHOWN: u8 = 103;
    pub const ADD_MONITOR: u8 = 104;
    pub const REMOVE_MONITOR: u8 = 105;
    pub const SET_PRIMARY: u8 = 106;
    pub const SET_MIRROR: u8 = 107;
    pub const SET_ORDER: u8 = 108;
    pub const PAIR_BEGIN: u8 = 109;
    pub const PAIR_MSG_CLIENT: u8 = 110;
    pub const PAIR_CONFIRM: u8 = 111;
    pub const SET_AUDIO: u8 = 112;
    pub const SET_MIC: u8 = 113;
    pub const MIC: u8 = 114;
    pub const PRESENCE: u8 = 115;
    pub const SET_MONITORS: u8 = 116;
    pub const SET_LAYOUT: u8 = 117;
    pub const SET_PC_SETTINGS: u8 = 118;
    pub const BATTERY: u8 = 119;
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn write_frame(w: &mut impl Write, t: u8, parts: &[&[u8]]) -> io::Result<()> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let len = u32::try_from(len).map_err(|_| invalid("payload too large"))?;
    let mut header = [0u8; 5];
    header[0] = t;
    header[1..].copy_from_slice(&len.to_le_bytes());
    w.write_all(&header)?;
    for p in parts {
        w.write_all(p)?;
    }
    Ok(())
}

fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header[1..].try_into().unwrap());
    if len > MAX_PAYLOAD {
        return Err(invalid("payload too large"));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.0.len() < n {
            return Err(invalid("truncated message"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn rest(self) -> &'a [u8] {
        self.0
    }
}

impl HostMsg {
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            // Never sent: only stands for what a newer peer sent.
            HostMsg::Unknown { .. } => Ok(()),
            HostMsg::Welcome { version, language } => write_frame(
                w,
                tag::WELCOME,
                &[&version.to_le_bytes(), language.as_bytes()],
            ),
            HostMsg::StreamStart {
                id,
                width,
                height,
                codec,
                name,
            } => write_frame(
                w,
                tag::STREAM_START,
                &[
                    &[*id, *codec as u8],
                    &width.to_le_bytes(),
                    &height.to_le_bytes(),
                    name.as_bytes(),
                ],
            ),
            HostMsg::Frame {
                id,
                pts_us,
                keyframe,
                data,
            } => write_frame(
                w,
                tag::FRAME,
                &[&[*id, *keyframe as u8], &pts_us.to_le_bytes(), data],
            ),
            HostMsg::StreamEnd { id } => write_frame(w, tag::STREAM_END, &[&[*id]]),
            HostMsg::PrimaryChanged { id } => write_frame(w, tag::PRIMARY_CHANGED, &[&[*id]]),
            HostMsg::Error { text } => write_frame(w, tag::ERROR, &[text.as_bytes()]),
            HostMsg::Order { ids } => write_frame(w, tag::ORDER, &[ids]),
            HostMsg::PairingRequired => write_frame(w, tag::PAIRING_REQUIRED, &[]),
            HostMsg::Audio { pcm } => write_frame(w, tag::AUDIO, &[pcm]),
            HostMsg::Focus { id } => write_frame(w, tag::FOCUS, &[&[*id]]),
            HostMsg::PcSettings { settings, encoders } => {
                write_frame(w, tag::PC_SETTINGS, &[&settings.bytes(), &[*encoders]])
            }
            HostMsg::Recenter => write_frame(w, tag::RECENTER, &[]),
            HostMsg::Incompatible { protocol, app } => write_frame(
                w,
                tag::INCOMPATIBLE,
                &[&protocol.to_le_bytes(), app.as_bytes()],
            ),
            HostMsg::MicWanted { enabled } => write_frame(w, tag::MIC_WANTED, &[&[*enabled as u8]]),
            HostMsg::MicActive { active } => write_frame(w, tag::MIC_ACTIVE, &[&[*active as u8]]),
            HostMsg::AudioWanted { enabled } => {
                write_frame(w, tag::AUDIO_WANTED, &[&[*enabled as u8]])
            }
            HostMsg::PairMsg { msg } => write_frame(w, tag::PAIR_MSG_HOST, &[msg]),
            HostMsg::PairResult { ok, tag } => {
                write_frame(w, tag::PAIR_RESULT, &[&[*ok as u8], tag])
            }
            HostMsg::MirrorWanted { enabled } => {
                write_frame(w, tag::MIRROR_WANTED, &[&[*enabled as u8]])
            }
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<Self> {
        let (t, payload) = read_frame(r)?;
        let mut c = Cursor(&payload);
        Ok(match t {
            tag::WELCOME => HostMsg::Welcome {
                version: c.u32()?,
                language: String::from_utf8_lossy(c.rest()).into_owned(),
            },
            tag::STREAM_START => {
                let id = c.u8()?;
                // A codec from a newer PC: skip the stream, keep the connection.
                let Some(codec) = Codec::from_u8(c.u8()?) else {
                    return Ok(HostMsg::Unknown { tag: t });
                };
                let width = c.u32()?;
                let height = c.u32()?;
                let name = String::from_utf8_lossy(c.rest()).into_owned();
                HostMsg::StreamStart {
                    id,
                    width,
                    height,
                    codec,
                    name,
                }
            }
            tag::FRAME => {
                let id = c.u8()?;
                let keyframe = c.u8()? != 0;
                let pts_us = c.u64()?;
                let data = c.rest().to_vec();
                HostMsg::Frame {
                    id,
                    pts_us,
                    keyframe,
                    data,
                }
            }
            tag::STREAM_END => HostMsg::StreamEnd { id: c.u8()? },
            tag::PRIMARY_CHANGED => HostMsg::PrimaryChanged { id: c.u8()? },
            tag::PAIRING_REQUIRED => HostMsg::PairingRequired,
            tag::FOCUS => HostMsg::Focus { id: c.u8()? },
            tag::RECENTER => HostMsg::Recenter,
            tag::PC_SETTINGS => HostMsg::PcSettings {
                settings: PcSettings::read(&mut c)?,
                encoders: c.u8()?,
            },
            tag::INCOMPATIBLE => HostMsg::Incompatible {
                protocol: c.u32()?,
                app: String::from_utf8_lossy(c.rest()).into_owned(),
            },
            tag::MIC_WANTED => HostMsg::MicWanted {
                enabled: c.u8()? != 0,
            },
            tag::MIC_ACTIVE => HostMsg::MicActive {
                active: c.u8()? != 0,
            },
            tag::AUDIO => HostMsg::Audio {
                pcm: c.rest().to_vec(),
            },
            tag::AUDIO_WANTED => HostMsg::AudioWanted {
                enabled: c.u8()? != 0,
            },
            tag::PAIR_MSG_HOST => HostMsg::PairMsg {
                msg: c.rest().to_vec(),
            },
            tag::PAIR_RESULT => HostMsg::PairResult {
                ok: c.u8()? != 0,
                tag: c.rest().to_vec(),
            },
            tag::ORDER => HostMsg::Order {
                ids: c.rest().to_vec(),
            },
            tag::MIRROR_WANTED => HostMsg::MirrorWanted {
                enabled: c.u8()? != 0,
            },
            tag::ERROR => HostMsg::Error {
                text: String::from_utf8_lossy(c.rest()).into_owned(),
            },
            // Newer PCs may send messages this build does not know.
            _ => HostMsg::Unknown { tag: t },
        })
    }
}

impl ClientMsg {
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            ClientMsg::Hello { version, min, app } => write_frame(
                w,
                tag::HELLO,
                &[&version.to_le_bytes(), &min.to_le_bytes(), app.as_bytes()],
            ),
            // Never sent: only stands for what a newer peer sent.
            ClientMsg::Unknown { .. } => Ok(()),
            ClientMsg::RequestKeyframe { id } => write_frame(w, tag::REQUEST_KEYFRAME, &[&[*id]]),
            ClientMsg::FrameShown { id, pts_us } => {
                write_frame(w, tag::FRAME_SHOWN, &[&[*id], &pts_us.to_le_bytes()])
            }
            ClientMsg::AddMonitor { width, height } => write_frame(
                w,
                tag::ADD_MONITOR,
                &[&width.to_le_bytes(), &height.to_le_bytes()],
            ),
            ClientMsg::RemoveMonitor { id } => write_frame(w, tag::REMOVE_MONITOR, &[&[*id]]),
            ClientMsg::SetPrimary { id } => write_frame(w, tag::SET_PRIMARY, &[&[*id]]),
            ClientMsg::SetOrder { ids } => write_frame(w, tag::SET_ORDER, &[ids]),
            ClientMsg::SetLayout { rows } => {
                let mut body = Vec::new();
                for row in rows {
                    body.push(row.ids.len() as u8);
                    for (i, id) in row.ids.iter().enumerate() {
                        body.push(*id);
                        let c = row.centers.get(i).copied().unwrap_or(NEXT);
                        body.extend_from_slice(&c.to_le_bytes());
                    }
                }
                write_frame(w, tag::SET_LAYOUT, &[&body])
            }
            ClientMsg::SetMonitors { sizes, primary } => {
                let mut body = vec![*primary];
                for (width, height) in sizes {
                    body.extend_from_slice(&width.to_le_bytes());
                    body.extend_from_slice(&height.to_le_bytes());
                }
                write_frame(w, tag::SET_MONITORS, &[&body])
            }
            ClientMsg::PairBegin {
                headset_shows,
                name,
            } => write_frame(
                w,
                tag::PAIR_BEGIN,
                &[&[*headset_shows as u8], name.as_bytes()],
            ),
            ClientMsg::PairMsg { msg } => write_frame(w, tag::PAIR_MSG_CLIENT, &[msg]),
            ClientMsg::SetAudio { enabled } => write_frame(w, tag::SET_AUDIO, &[&[*enabled as u8]]),
            ClientMsg::SetMic { enabled } => write_frame(w, tag::SET_MIC, &[&[*enabled as u8]]),
            ClientMsg::Mic { pcm } => write_frame(w, tag::MIC, &[pcm]),
            ClientMsg::Presence { worn } => write_frame(w, tag::PRESENCE, &[&[*worn as u8]]),
            ClientMsg::SetPcSettings { settings } => {
                write_frame(w, tag::SET_PC_SETTINGS, &[&settings.bytes()])
            }
            ClientMsg::Battery { percent, charging } => {
                write_frame(w, tag::BATTERY, &[&[*percent, *charging as u8]])
            }
            ClientMsg::PairConfirm { tag } => write_frame(w, tag::PAIR_CONFIRM, &[tag]),
            ClientMsg::SetMirror { enabled } => {
                write_frame(w, tag::SET_MIRROR, &[&[*enabled as u8]])
            }
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<Self> {
        let (t, payload) = read_frame(r)?;
        let mut c = Cursor(&payload);
        Ok(match t {
            tag::HELLO => ClientMsg::Hello {
                version: c.u32()?,
                min: c.u32()?,
                app: String::from_utf8_lossy(c.rest()).into_owned(),
            },
            tag::REQUEST_KEYFRAME => ClientMsg::RequestKeyframe { id: c.u8()? },
            tag::FRAME_SHOWN => ClientMsg::FrameShown {
                id: c.u8()?,
                pts_us: c.u64()?,
            },
            tag::ADD_MONITOR => ClientMsg::AddMonitor {
                width: c.u32()?,
                height: c.u32()?,
            },
            tag::REMOVE_MONITOR => ClientMsg::RemoveMonitor { id: c.u8()? },
            tag::SET_PRIMARY => ClientMsg::SetPrimary { id: c.u8()? },
            tag::PAIR_BEGIN => ClientMsg::PairBegin {
                headset_shows: c.u8()? != 0,
                name: String::from_utf8_lossy(c.rest()).into_owned(),
            },
            tag::SET_MIC => ClientMsg::SetMic {
                enabled: c.u8()? != 0,
            },
            tag::MIC => ClientMsg::Mic {
                pcm: c.rest().to_vec(),
            },
            tag::PRESENCE => ClientMsg::Presence { worn: c.u8()? != 0 },
            tag::BATTERY => ClientMsg::Battery {
                percent: c.u8()?,
                charging: c.u8()? != 0,
            },
            tag::SET_PC_SETTINGS => ClientMsg::SetPcSettings {
                settings: PcSettings::read(&mut c)?,
            },
            tag::SET_AUDIO => ClientMsg::SetAudio {
                enabled: c.u8()? != 0,
            },
            tag::PAIR_MSG_CLIENT => ClientMsg::PairMsg {
                msg: c.rest().to_vec(),
            },
            tag::PAIR_CONFIRM => ClientMsg::PairConfirm {
                tag: c.rest().to_vec(),
            },
            tag::SET_LAYOUT => {
                let mut rows = Vec::new();
                while let Ok(n) = c.u8() {
                    let mut row = LayoutRow {
                        ids: Vec::new(),
                        centers: Vec::new(),
                    };
                    for _ in 0..n {
                        row.ids.push(c.u8()?);
                        row.centers.push(c.u32()? as i32);
                    }
                    rows.push(row);
                }
                ClientMsg::SetLayout { rows }
            }
            tag::SET_MONITORS => {
                let primary = c.u8()?;
                let mut sizes = Vec::new();
                while let (Ok(width), Ok(height)) = (c.u32(), c.u32()) {
                    sizes.push((width, height));
                }
                ClientMsg::SetMonitors { sizes, primary }
            }
            tag::SET_ORDER => ClientMsg::SetOrder {
                ids: c.rest().to_vec(),
            },
            tag::SET_MIRROR => ClientMsg::SetMirror {
                enabled: c.u8()? != 0,
            },
            _ => ClientMsg::Unknown { tag: t },
        })
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
mod tests {
    use super::*;

    /// A message from a newer peer is skipped and the next one still reads.
    #[test]
    fn skips_unknown_messages() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 250, &[&[1, 2, 3]]).unwrap();
        HostMsg::Focus { id: 1 }.write(&mut buf).unwrap();
        // A stream in a codec this build does not know.
        write_frame(&mut buf, tag::STREAM_START, &[&[0, 9], &[0; 8], b"x"]).unwrap();
        let mut r = buf.as_slice();
        assert_eq!(
            HostMsg::read(&mut r).unwrap(),
            HostMsg::Unknown { tag: 250 }
        );
        assert_eq!(HostMsg::read(&mut r).unwrap(), HostMsg::Focus { id: 1 });
        assert!(matches!(
            HostMsg::read(&mut r).unwrap(),
            HostMsg::Unknown { .. }
        ));

        let mut buf = Vec::new();
        write_frame(&mut buf, 251, &[&[7; 10]]).unwrap();
        ClientMsg::SetMic { enabled: true }.write(&mut buf).unwrap();
        let mut r = buf.as_slice();
        assert_eq!(
            ClientMsg::read(&mut r).unwrap(),
            ClientMsg::Unknown { tag: 251 }
        );
        assert_eq!(
            ClientMsg::read(&mut r).unwrap(),
            ClientMsg::SetMic { enabled: true }
        );
    }

    #[test]
    fn version_ranges() {
        assert!(compatible(PROTOCOL_VERSION, MIN_PROTOCOL));
        // A newer peer that still understands us.
        assert!(compatible(PROTOCOL_VERSION + 3, PROTOCOL_VERSION));
        // A newer peer that dropped what we speak.
        assert!(!compatible(PROTOCOL_VERSION + 3, PROTOCOL_VERSION + 1));
        // An older peer than we still support.
        assert!(!compatible(MIN_PROTOCOL - 1, 0));
    }

    #[test]
    fn roundtrip() {
        let msgs = vec![
            HostMsg::Welcome {
                version: 1,
                language: "uk".into(),
            },
            HostMsg::StreamStart {
                id: 2,
                width: 2560,
                height: 1440,
                codec: Codec::H265,
                name: "Prostir-1".into(),
            },
            HostMsg::Frame {
                id: 2,
                pts_us: 123456789,
                keyframe: true,
                data: vec![0, 0, 0, 1, 0x40],
            },
            HostMsg::StreamEnd { id: 2 },
            HostMsg::PrimaryChanged { id: 1 },
            HostMsg::MirrorWanted { enabled: true },
            HostMsg::Order { ids: vec![0, 3, 1] },
            HostMsg::PairingRequired,
            HostMsg::Audio {
                pcm: vec![0, 1, 2, 3],
            },
            HostMsg::AudioWanted { enabled: false },
            HostMsg::MicWanted { enabled: true },
            HostMsg::MicActive { active: true },
            HostMsg::Focus { id: 2 },
            HostMsg::Recenter,
            HostMsg::PcSettings {
                settings: PcSettings {
                    quality: 30,
                    codec: Codec::H264,
                    software: false,
                    sharp: true,
                },
                encoders: encoders::GPU_H264 | encoders::GPU_H265,
            },
            HostMsg::Incompatible {
                protocol: 10,
                app: "0.3.0".into(),
            },
            HostMsg::PairMsg { msg: vec![4, 5] },
            HostMsg::PairResult {
                ok: true,
                tag: vec![1; 32],
            },
            HostMsg::Error {
                text: "Не вдалося".into(),
            },
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            m.write(&mut buf).unwrap();
        }
        let mut r = buf.as_slice();
        for m in &msgs {
            assert_eq!(&HostMsg::read(&mut r).unwrap(), m);
        }

        let mut buf = Vec::new();
        ClientMsg::RequestKeyframe { id: 7 }
            .write(&mut buf)
            .unwrap();
        assert_eq!(
            ClientMsg::read(&mut buf.as_slice()).unwrap(),
            ClientMsg::RequestKeyframe { id: 7 }
        );

        let mut buf = Vec::new();
        let shown = ClientMsg::FrameShown {
            id: 1,
            pts_us: 3_600_000_000_123,
        };
        shown.write(&mut buf).unwrap();
        assert_eq!(ClientMsg::read(&mut buf.as_slice()).unwrap(), shown);

        for m in [
            ClientMsg::AddMonitor {
                width: 3440,
                height: 1440,
            },
            ClientMsg::RemoveMonitor { id: 2 },
            ClientMsg::SetPrimary { id: 1 },
            ClientMsg::SetMirror { enabled: true },
            ClientMsg::SetOrder { ids: vec![2, 0, 1] },
            ClientMsg::SetMonitors {
                sizes: vec![(2560, 1440), (1440, 2560)],
                primary: 1,
            },
            ClientMsg::SetLayout {
                rows: vec![
                    LayoutRow {
                        ids: vec![3],
                        centers: vec![-1280],
                    },
                    LayoutRow {
                        ids: vec![0, 1, 2],
                        centers: vec![-2560, 0, NEXT],
                    },
                ],
            },
            ClientMsg::PairBegin {
                headset_shows: true,
                name: "Meta Quest 3".into(),
            },
            ClientMsg::PairMsg { msg: vec![1, 2, 3] },
            ClientMsg::PairConfirm { tag: vec![9; 32] },
            ClientMsg::SetAudio { enabled: true },
            ClientMsg::SetMic { enabled: false },
            ClientMsg::Mic {
                pcm: vec![1, 0, 2, 0],
            },
            ClientMsg::Presence { worn: true },
            ClientMsg::Battery {
                percent: 72,
                charging: true,
            },
            ClientMsg::SetPcSettings {
                settings: PcSettings {
                    quality: 50,
                    codec: Codec::H265,
                    software: true,
                    sharp: false,
                },
            },
        ] {
            let mut buf = Vec::new();
            m.write(&mut buf).unwrap();
            assert_eq!(ClientMsg::read(&mut buf.as_slice()).unwrap(), m);
        }

        let b = encode_beacon(47830, "desk");
        assert_eq!(decode_beacon(&b), Some((47830, "desk".into())));
    }
}
