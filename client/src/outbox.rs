//! Messages from the headset to the PC, most important first. Sending only queues
//! them: one writer thread puts them on the network packet by packet, so the frame
//! loop, the microphone and the decoders never wait on Wi-Fi.
//!
//! Two classes, each in a lane of its own:
//! - control (requests, settings, the layout, pairing): always sent, in order;
//! - microphone sound and the "frame shown" acknowledgements: sent after control, and
//!   dropped while the network is stuck (sound would be late anyway, the
//!   acknowledgements are for latency figures only).

use std::collections::VecDeque;
use std::io;
use std::sync::{Condvar, Mutex};

use prostir_proto::ClientMsg;
use prostir_proto::link::RecordSender;
use prostir_proto::packet::{Encoded, FRAGMENT, Outgoing, lane};

/// About 2.5 s of microphone sound: more than that waiting means the network is stuck.
pub const MAX_LOSSY: usize = 256;

/// Messages it costs little to lose when the network is stuck.
fn lossy(msg: &ClientMsg) -> bool {
    matches!(msg, ClientMsg::Mic { .. } | ClientMsg::FrameShown { .. })
}

#[derive(Default)]
struct State {
    control: VecDeque<Encoded>,
    lossy: VecDeque<Encoded>,
    closed: bool,
}

#[derive(Default)]
pub struct Outbox {
    state: Mutex<State>,
    ready: Condvar,
}

/// The lane the writer sends a packet in next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Control,
    Lossy,
}

/// The messages the writer has started: at most one per lane.
#[derive(Default)]
pub struct Lanes {
    control: Option<Outgoing>,
    lossy: Option<Outgoing>,
}

impl Lanes {
    /// Sends the next packet in `pick`'s lane.
    pub fn send(&mut self, pick: Pick, tx: &mut dyn RecordSender) -> io::Result<()> {
        let out = match pick {
            Pick::Control => &mut self.control,
            Pick::Lossy => &mut self.lossy,
        };
        if let Some(o) = out
            && o.send_next(tx, FRAGMENT)?
        {
            *out = None;
        }
        Ok(())
    }
}

impl Outbox {
    /// Queues `msg`. A lossy message beyond [`MAX_LOSSY`] waiting is dropped. An error
    /// means the session is over, or the message cannot be sent at all.
    pub fn push(&self, msg: ClientMsg) -> io::Result<()> {
        let is_lossy = lossy(&msg);
        let msg = msg.encode()?;
        let mut s = self.state.lock().unwrap();
        if s.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if is_lossy {
            if s.lossy.len() >= MAX_LOSSY {
                return Ok(());
            }
            s.lossy.push_back(msg);
        } else {
            s.control.push_back(msg);
        }
        self.ready.notify_one();
        Ok(())
    }

    /// The session is over: nothing more is taken or sent.
    pub fn close(&self) {
        let mut s = self.state.lock().unwrap();
        s.closed = true;
        s.control.clear();
        s.lossy.clear();
        self.ready.notify_all();
    }

    /// The lane to send the next packet in, waiting until there is one; `None` once
    /// closed. A message taken from the queues moves into `lanes`.
    pub fn next(&self, lanes: &mut Lanes) -> Option<Pick> {
        let mut s = self.state.lock().unwrap();
        loop {
            if s.closed {
                return None;
            }
            if let Some(pick) = s.pick(lanes) {
                return Some(pick);
            }
            s = self.ready.wait(s).unwrap();
        }
    }
}

impl State {
    fn pick(&mut self, lanes: &mut Lanes) -> Option<Pick> {
        if lanes.control.is_none()
            && let Some(msg) = self.control.pop_front()
        {
            lanes.control = Some(Outgoing::new(lane::CONTROL, msg));
        }
        if lanes.control.is_some() {
            return Some(Pick::Control);
        }
        if lanes.lossy.is_none()
            && let Some(msg) = self.lossy.pop_front()
        {
            lanes.lossy = Some(Outgoing::new(lane::SOUND, msg));
        }
        lanes.lossy.is_some().then_some(Pick::Lossy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prostir_proto::link::memory;

    /// Sends everything waiting through a link and reads it back as the PC would.
    fn sent(o: &Outbox) -> Vec<(u8, ClientMsg)> {
        let (mut headset, mut pc) = memory::pair(4096);
        let mut lanes = Lanes::default();
        o.drain_into(&mut lanes, &mut *headset.sender);
        headset.closer.close();
        let mut asm = ClientMsg::assembler();
        asm.set_limits(ClientMsg::limits(true));
        let mut out = Vec::new();
        while let Ok(record) = pc.receiver.recv() {
            let lane = record[1];
            if let Some(m) = asm.push(record).unwrap() {
                out.push((lane, ClientMsg::from_message(m).unwrap().unwrap()));
            }
        }
        out
    }

    impl Outbox {
        /// Sends what is waiting, without blocking.
        fn drain_into(&self, lanes: &mut Lanes, tx: &mut dyn RecordSender) {
            loop {
                let pick = self.state.lock().unwrap().pick(lanes);
                match pick {
                    Some(pick) => lanes.send(pick, tx).unwrap(),
                    None => break,
                }
            }
        }
    }

    #[test]
    fn control_goes_first_and_each_class_has_its_lane() {
        let o = Outbox::default();
        o.push(ClientMsg::Mic { pcm: vec![1; 960] }).unwrap();
        o.push(ClientMsg::FrameShown { id: 0, pts_us: 5 }).unwrap();
        o.push(ClientMsg::SetMic { enabled: true }).unwrap();
        assert_eq!(
            sent(&o),
            [
                (lane::CONTROL, ClientMsg::SetMic { enabled: true }),
                (lane::SOUND, ClientMsg::Mic { pcm: vec![1; 960] }),
                (lane::SOUND, ClientMsg::FrameShown { id: 0, pts_us: 5 }),
            ]
        );
    }

    /// While the network is stuck, sound and acknowledgements beyond a limit are
    /// dropped; control messages never are.
    #[test]
    fn only_lossy_messages_are_dropped() {
        let o = Outbox::default();
        for _ in 0..MAX_LOSSY + 10 {
            o.push(ClientMsg::Mic { pcm: vec![0; 2] }).unwrap();
        }
        for i in 0..MAX_LOSSY + 10 {
            let id = (i % 100) as u8;
            o.push(ClientMsg::RemoveMonitor { id }).unwrap();
        }
        let msgs = sent(&o);
        let mic = msgs.iter().filter(|(l, _)| *l == lane::SOUND).count();
        assert_eq!(mic, MAX_LOSSY);
        assert_eq!(msgs.len() - mic, MAX_LOSSY + 10);
    }

    #[test]
    fn nothing_is_taken_once_closed() {
        let o = Outbox::default();
        o.close();
        assert!(o.push(ClientMsg::Open).is_err());
        assert_eq!(o.next(&mut Lanes::default()), None);
        // Nor what cannot be sent at all.
        let o = Outbox::default();
        let long = ClientMsg::PairBegin {
            headset_shows: true,
            name: "x".repeat(65),
        };
        assert_eq!(
            o.push(long).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
