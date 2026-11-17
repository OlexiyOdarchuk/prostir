//! Wire protocol between the PC host and the Quest client.
//!
//! Every message is framed as `[tag: u8][len: u32 LE][payload: len bytes]`.
//! The client connects over TCP (USB via `adb reverse`, or Wi-Fi after
//! discovering the host through a UDP broadcast beacon).

use std::io::{self, Read, Write};

pub const TCP_PORT: u16 = 47830;
pub const DISCOVERY_PORT: u16 = 47831;
pub const PROTOCOL_VERSION: u32 = 2;
/// Prefix of the UDP discovery beacon: `MAGIC ++ tcp_port (u16 LE) ++ hostname (utf-8)`.
pub const BEACON_MAGIC: &[u8; 8] = b"QMONHOST";

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
    fn from_u8(v: u8) -> io::Result<Self> {
        match v {
            0 => Ok(Codec::H264),
            1 => Ok(Codec::H265),
            _ => Err(invalid("unknown codec")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HostMsg {
    /// Sent once after the handshake.
    Welcome {
        version: u32,
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientMsg {
    Hello {
        version: u32,
    },
    RequestKeyframe {
        id: u8,
    },
    /// The decoder handed the frame with this `pts_us` to the compositor.
    FrameShown {
        id: u8,
        pts_us: u64,
    },
}

mod tag {
    pub const WELCOME: u8 = 1;
    pub const STREAM_START: u8 = 2;
    pub const FRAME: u8 = 3;
    pub const STREAM_END: u8 = 4;
    pub const HELLO: u8 = 101;
    pub const REQUEST_KEYFRAME: u8 = 102;
    pub const FRAME_SHOWN: u8 = 103;
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
            HostMsg::Welcome { version } => write_frame(w, tag::WELCOME, &[&version.to_le_bytes()]),
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
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<Self> {
        let (t, payload) = read_frame(r)?;
        let mut c = Cursor(&payload);
        Ok(match t {
            tag::WELCOME => HostMsg::Welcome { version: c.u32()? },
            tag::STREAM_START => {
                let id = c.u8()?;
                let codec = Codec::from_u8(c.u8()?)?;
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
            _ => return Err(invalid("unknown host message")),
        })
    }
}

impl ClientMsg {
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            ClientMsg::Hello { version } => write_frame(w, tag::HELLO, &[&version.to_le_bytes()]),
            ClientMsg::RequestKeyframe { id } => write_frame(w, tag::REQUEST_KEYFRAME, &[&[*id]]),
            ClientMsg::FrameShown { id, pts_us } => {
                write_frame(w, tag::FRAME_SHOWN, &[&[*id], &pts_us.to_le_bytes()])
            }
        }
    }

    pub fn read(r: &mut impl Read) -> io::Result<Self> {
        let (t, payload) = read_frame(r)?;
        let mut c = Cursor(&payload);
        Ok(match t {
            tag::HELLO => ClientMsg::Hello { version: c.u32()? },
            tag::REQUEST_KEYFRAME => ClientMsg::RequestKeyframe { id: c.u8()? },
            tag::FRAME_SHOWN => ClientMsg::FrameShown {
                id: c.u8()?,
                pts_us: c.u64()?,
            },
            _ => return Err(invalid("unknown client message")),
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

    #[test]
    fn roundtrip() {
        let msgs = vec![
            HostMsg::Welcome { version: 1 },
            HostMsg::StreamStart {
                id: 2,
                width: 2560,
                height: 1440,
                codec: Codec::H265,
                name: "Quest-1".into(),
            },
            HostMsg::Frame {
                id: 2,
                pts_us: 123456789,
                keyframe: true,
                data: vec![0, 0, 0, 1, 0x40],
            },
            HostMsg::StreamEnd { id: 2 },
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

        let b = encode_beacon(47830, "desk");
        assert_eq!(decode_beacon(&b), Some((47830, "desk".into())));
    }
}
