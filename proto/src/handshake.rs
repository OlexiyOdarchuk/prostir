//! The frozen version handshake on top of records, and where packets begin.
//!
//! [`ClientMsg::Hello`], [`HostMsg::Welcome`], [`HostMsg::Incompatible`] and
//! [`HostMsg::Error`] go in the stream format of protocol 1, byte for byte, so any
//! version, older or newer, reads them as it always did: a reader joins records into
//! one byte stream and reads messages off it, across record boundaries. What a new
//! version adds is where that stream ends:
//!
//! * the headset puts its hello in a record of its own, and nothing else in it; once
//!   the versions fit, the PC requires that (a hello sharing its record would leave
//!   bytes the packets after it could not account for);
//! * the PC puts its welcome in a record of its own;
//! * everything after those two records is packets ([`crate::packet`]).
//!
//! `Error` and `Incompatible` end a connection whose versions do not fit, so they stay
//! in the stream format.

use std::io::{self, Write};

use crate::link::{RecordReceiver, RecordSender, StreamReader, StreamWriter};
use crate::{ClientMsg, HostMsg, MIN_PROTOCOL, PROTOCOL_VERSION};

/// The headset's hello, in a record of its own.
pub fn send_hello(tx: &mut dyn RecordSender, app: &str) -> io::Result<()> {
    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        min: MIN_PROTOCOL,
        app: app.into(),
    };
    send_frozen(&mut StreamWriter(tx), |w| hello.write(w))
}

/// A frozen message from the PC (`Welcome`, `Error`, `Incompatible`), in records of
/// its own.
pub fn send_from_pc(tx: &mut dyn RecordSender, msg: &HostMsg) -> io::Result<()> {
    send_frozen(&mut StreamWriter(tx), |w| msg.write(w))
}

/// The message written whole, then sent: a message under a record's size is one record.
fn send_frozen(
    w: &mut StreamWriter,
    write: impl FnOnce(&mut Vec<u8>) -> io::Result<()>,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    write(&mut bytes)?;
    w.write_all(&bytes)
}

/// What the headset said first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum First {
    Hello {
        version: u32,
        min: u32,
        app: String,
        /// It ended its record: packets may follow.
        alone: bool,
    },
    /// Something else, skipped unread: only a headset from before protocol 2 starts
    /// so (it may ask to pair before saying hello).
    Other,
}

/// The PC reads the headset's first message.
pub fn read_first(rx: &mut dyn RecordReceiver) -> io::Result<First> {
    let mut stream = StreamReader::new(rx);
    Ok(match ClientMsg::read_first(&mut stream)? {
        Some(ClientMsg::Hello { version, min, app }) => First::Hello {
            version,
            min,
            app,
            alone: stream.at_record_end(),
        },
        _ => First::Other,
    })
}

/// The PC's answer to the hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The versions fit; packets follow.
    Welcome { version: u32, language: String },
    /// They do not; the PC closes the connection.
    Incompatible { protocol: u32, app: String },
}

/// The headset reads the PC's answer. The text of an `Error` before `Incompatible` is
/// for headsets older than `Incompatible`, and skipped. A welcome that does not end
/// its record is an error: the packets after it would start in the middle.
pub fn read_answer(rx: &mut dyn RecordReceiver) -> io::Result<Answer> {
    let mut stream = StreamReader::new(rx);
    loop {
        match HostMsg::read_setup(&mut stream)? {
            HostMsg::Welcome { version, language } if stream.at_record_end() => {
                return Ok(Answer::Welcome { version, language });
            }
            HostMsg::Welcome { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the PC's welcome shares its record with more",
                ));
            }
            HostMsg::Incompatible { protocol, app } => {
                return Ok(Answer::Incompatible { protocol, app });
            }
            HostMsg::Error { .. } => {}
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected {other:?} before the welcome"),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::memory;
    use crate::packet::{self, lane};

    /// The hello and the welcome keep the bytes of protocol 1, each in its record, and
    /// the first packet comes right after them.
    #[test]
    fn hello_and_welcome_end_their_records() {
        let (mut headset, mut pc) = memory::pair(16);
        // The hello, byte for byte as protocol 1 wrote it, alone in its record.
        send_hello(&mut *headset.sender, "0.2.0").unwrap();
        let mut expected = vec![101, 13, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0];
        expected.extend_from_slice(b"0.2.0");
        assert_eq!(pc.receiver.recv().unwrap(), expected);
        send_hello(&mut *headset.sender, "0.2.0").unwrap();
        let open = ClientMsg::Open.encode().unwrap();
        packet::send_whole(&mut *headset.sender, lane::CONTROL, open, packet::FRAGMENT).unwrap();
        assert_eq!(
            read_first(&mut *pc.receiver).unwrap(),
            First::Hello {
                version: PROTOCOL_VERSION,
                min: MIN_PROTOCOL,
                app: "0.2.0".into(),
                alone: true
            }
        );
        let mut packets = ClientMsg::assembler();
        assert_eq!(
            ClientMsg::recv(&mut *pc.receiver, &mut packets).unwrap(),
            ClientMsg::Open
        );

        let welcome = HostMsg::Welcome {
            version: PROTOCOL_VERSION,
            language: "uk".into(),
        };
        send_from_pc(&mut *pc.sender, &welcome).unwrap();
        let opened = HostMsg::Opened.encode().unwrap();
        packet::send_whole(&mut *pc.sender, lane::CONTROL, opened, packet::FRAGMENT).unwrap();
        assert_eq!(
            read_answer(&mut *headset.receiver).unwrap(),
            Answer::Welcome {
                version: PROTOCOL_VERSION,
                language: "uk".into()
            }
        );
        let mut packets = HostMsg::assembler();
        assert_eq!(
            HostMsg::recv(&mut *headset.receiver, &mut packets).unwrap(),
            HostMsg::Opened
        );
    }

    /// A hello with more after it in its record is told apart, and so is a protocol 1
    /// headset asking to pair first.
    #[test]
    fn the_pc_sees_what_came_first() {
        let (mut headset, mut pc) = memory::pair(16);
        let mut hello = Vec::new();
        ClientMsg::Hello {
            version: 2,
            min: 2,
            app: "x".into(),
        }
        .write(&mut hello)
        .unwrap();
        headset.sender.send(&[&hello, &[0, 0]]).unwrap();
        assert!(matches!(
            read_first(&mut *pc.receiver).unwrap(),
            First::Hello { alone: false, .. }
        ));
        // Protocol 1 pairing first: whether the headset shows the code, its name.
        let mut old = vec![109, 11, 0, 0, 0, 1];
        old.extend_from_slice(b"Meta Quest");
        headset.sender.send(&[&old]).unwrap();
        assert_eq!(read_first(&mut *pc.receiver).unwrap(), First::Other);
    }

    /// `Error` and `Incompatible` reach the headset across any record boundaries, as
    /// any version writes them; a welcome sharing its record does not.
    #[test]
    fn the_headset_reads_a_refusal_however_it_is_cut() {
        let mut bytes = Vec::new();
        HostMsg::Error {
            text: "Update Prostir in the headset".into(),
        }
        .write(&mut bytes)
        .unwrap();
        HostMsg::Incompatible {
            protocol: 3,
            app: "0.3.0".into(),
        }
        .write(&mut bytes)
        .unwrap();
        for cut in [1, 5, 7, bytes.len() - 1] {
            let (mut pc, mut headset) = memory::pair(16);
            pc.sender.send(&[&bytes[..cut]]).unwrap();
            pc.sender.send(&[&bytes[cut..]]).unwrap();
            assert_eq!(
                read_answer(&mut *headset.receiver).unwrap(),
                Answer::Incompatible {
                    protocol: 3,
                    app: "0.3.0".into()
                }
            );
        }
        let (mut pc, mut headset) = memory::pair(16);
        let mut welcome = Vec::new();
        HostMsg::Welcome {
            version: 2,
            language: "en".into(),
        }
        .write(&mut welcome)
        .unwrap();
        pc.sender.send(&[&welcome, &[1]]).unwrap();
        let err = read_answer(&mut *headset.receiver).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
