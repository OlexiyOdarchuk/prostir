//! The local API on the service's side: the requests, answers, events and state of
//! the crate `prostir-api` (shared with every client), and how they travel between
//! the control loop (`main.rs`) and a transport (D-Bus on Linux, see
//! `platform/local_api`; a named pipe on Windows later).
//!
//! A transport turns each request into `Event::Ctl` with a [`Reply`] of its own, and
//! sends out the [`Note`]s the control loop gives it, in order: a reply never goes out
//! before the `StateChanged` that shows what it changed. Nothing here knows the
//! transport.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use prostir_proto::state::ErrorCode;

pub use prostir_api::*;

/// What came of a request, before the transport words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlReply {
    Done,
    /// The display made for it.
    Added(u32),
    /// The pairing window opened, for so many seconds.
    Pairing(u32),
    /// A screenshot saved at `path`; `whole`: of the whole desktop.
    Saved {
        path: String,
        whole: bool,
    },
    Failed(Failure),
}

impl CtlReply {
    pub fn failed(error: ApiError, text: impl Into<String>) -> Self {
        Self::Failed(Failure::new(error, text))
    }

    /// What a headset request's outcome means here.
    pub fn of_outcome(outcome: &prostir_proto::request::Outcome) -> Self {
        use prostir_proto::request::Outcome;
        match outcome {
            Outcome::Added { id } => Self::Added(id.0),
            Outcome::Failed { error, detail } => Self::failed(of_code(*error), detail.clone()),
            _ => Self::Done,
        }
    }

    /// As it goes out, with the revision of the state that has the change.
    pub fn answer(&self, revision: u32) -> Answer {
        let mut answer = Answer {
            revision,
            ..Answer::default()
        };
        match self {
            Self::Done => {}
            Self::Added(id) => answer.id = Some(*id),
            Self::Pairing(seconds) => answer.seconds_left = Some(*seconds),
            Self::Saved { path, whole } => {
                answer.path = Some(path.clone());
                answer.whole = Some(*whole);
            }
            Self::Failed(f) => {
                answer.error = Some(f.error);
                answer.text = Some(f.text.clone());
            }
        }
        answer
    }
}

/// The local API's name for a protocol error code.
pub fn of_code(code: ErrorCode) -> ApiError {
    match code.known() {
        ErrorCode::UNSUPPORTED => ApiError::Unsupported,
        ErrorCode::NOT_ALLOWED => ApiError::NotAllowed,
        ErrorCode::UNKNOWN_DISPLAY => ApiError::UnknownDisplay,
        ErrorCode::LIMIT => ApiError::Limit,
        ErrorCode::BAD_SIZE => ApiError::BadSize,
        ErrorCode::LAST_DISPLAY => ApiError::LastDisplay,
        ErrorCode::NO_WINDOW => ApiError::NoWindow,
        ErrorCode::DESKTOP => ApiError::Desktop,
        ErrorCode::ENCODER => ApiError::Encoder,
        ErrorCode::CAPTURE => ApiError::Capture,
        ErrorCode::SOUND => ApiError::Sound,
        ErrorCode::NOT_ON_DISPLAYS => ApiError::NotOnDisplays,
        ErrorCode::NO_SUCH_PROFILE => ApiError::NoSuchProfile,
        ErrorCode::BAD_ARGUMENT => ApiError::BadArgument,
        _ => ApiError::Other,
    }
}

/// Where a request's reply goes, once there is one: the transport that took the
/// request made it. Answered exactly once; dropped unanswered, it answers `Other`, so
/// no client waits for ever.
pub struct Reply(Option<Box<dyn FnOnce(CtlReply) + Send>>);

impl Reply {
    pub fn new(send: impl FnOnce(CtlReply) + Send + 'static) -> Self {
        Self(Some(Box::new(send)))
    }

    pub fn send(mut self, reply: CtlReply) {
        if let Some(send) = self.0.take() {
            send(reply);
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(send) = self.0.take() {
            send(CtlReply::failed(ApiError::Other, "no answer"));
        }
    }
}

/// Replies ready to go, wherever they came from (a request carried out, a headset's
/// answer): they go out at the end of the control loop's pass, after its state.
#[derive(Clone, Default)]
pub struct Answers(Arc<Mutex<Vec<(Reply, CtlReply)>>>);

impl Answers {
    pub fn push(&self, reply: Reply, result: CtlReply) {
        self.0.lock().unwrap().push((reply, result));
    }

    /// Those ready now, in the order they came.
    pub fn take(&self) -> Vec<(Reply, CtlReply)> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// What the control loop gives the transports, in order.
pub enum Note {
    /// The state changed: its new revision, and the state.
    State(u32, Arc<State>),
    Event(Event),
    /// A reply to deliver, given the revision of the state already out.
    Answer(Box<dyn FnOnce(u32) + Send>),
}

/// The way to the transport, for the control loop (and the transport's replies).
#[derive(Clone)]
pub struct Outlet(Sender<Note>);

impl Outlet {
    pub fn send(&self, note: Note) {
        let _ = self.0.send(note);
    }
}

/// The channel from the control loop to the transport.
pub fn channel() -> (Outlet, Receiver<Note>) {
    let (tx, rx) = mpsc::channel();
    (Outlet(tx), rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prostir_proto::request::Outcome;
    use prostir_proto::state::DisplayId;

    /// A dropped reply still answers.
    #[test]
    fn a_dropped_reply_answers() {
        let (tx, rx) = mpsc::channel();
        drop(Reply::new(move |r| tx.send(r).unwrap()));
        assert_eq!(
            rx.recv().unwrap(),
            CtlReply::failed(ApiError::Other, "no answer")
        );
    }

    /// The headset's outcomes and codes under the local API's names.
    #[test]
    fn outcomes_and_codes() {
        assert_eq!(
            CtlReply::of_outcome(&Outcome::Added { id: DisplayId(7) }),
            CtlReply::Added(7)
        );
        let failed = Outcome::failed(ErrorCode::NO_SUCH_PROFILE, "Work");
        assert_eq!(
            CtlReply::of_outcome(&failed),
            CtlReply::failed(ApiError::NoSuchProfile, "Work")
        );
        assert_eq!(of_code(ErrorCode::from_code(200)), ApiError::Other);
        for n in 1..=14 {
            assert_ne!(of_code(ErrorCode::from_code(n)), ApiError::Other, "{n}");
        }
        assert_eq!(
            CtlReply::Added(3).answer(9).to_json(),
            r#"{"id":3,"revision":9}"#
        );
    }
}
