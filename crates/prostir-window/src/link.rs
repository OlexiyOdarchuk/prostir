//! The window's line to the service, off the window's thread, so a slow or hung
//! service never stops the window from drawing. Two threads share the client: one
//! follows the state (asks for it, takes the changes the service sends, asks again
//! when it appears or did not answer), the other makes the calls, so a call that
//! waits up to 35 s for the headset holds up no change meanwhile.
//!
//! In tests the same logic runs on the window's thread instead (`Link::inline`), so a
//! picture is taken of exactly the state that was asked for.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::api::{Answer, Change, Client, ClientError, Request, Sink, Snapshot};

/// What the window learns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// A state newer than any before.
    State(Snapshot),
    /// The service is not there (or cannot be reached, and why).
    Absent(ClientError),
    /// A request was answered.
    Replied {
        request: Request,
        result: Result<Answer, ClientError>,
    },
}

enum Job {
    Refresh,
    Changed(Change),
}

/// The longest pause between two tries of a service that does not answer.
const RETRY_MAX: Duration = Duration::from_secs(30);

/// The state side, the same on the thread and inline.
struct Follower {
    client: Arc<dyn Client>,
    revision: Option<u32>,
    /// Tries in a row the service was there and did not answer.
    failures: u32,
}

impl Follower {
    fn new(client: Arc<dyn Client>) -> Self {
        Self {
            client,
            revision: None,
            failures: 0,
        }
    }

    fn start(&mut self, sink: Sink) -> Vec<Update> {
        if let Err(e) = self.client.watch(sink) {
            return vec![Update::Absent(e)];
        }
        self.handle(Job::Refresh)
    }

    fn state(&mut self, s: Snapshot) -> Vec<Update> {
        self.failures = 0;
        // Signals and replies can cross: an older state never replaces a newer one. A
        // service that started again counts from the beginning, after Absent.
        if self.revision.is_some_and(|r| s.revision < r) {
            return Vec::new();
        }
        self.revision = Some(s.revision);
        vec![Update::State(s)]
    }

    fn absent(&mut self, e: ClientError) -> Vec<Update> {
        self.revision = None;
        // Asked again later only when it is there and did not answer; gone, the bus
        // says when it is back.
        self.failures = match e {
            ClientError::NotRunning | ClientError::NoBus(_) | ClientError::TooOld => 0,
            _ => self.failures + 1,
        };
        vec![Update::Absent(e)]
    }

    /// When to ask again, if the service did not answer.
    fn retry_in(&self) -> Option<Duration> {
        (self.failures > 0)
            .then(|| Duration::from_secs(1u64 << (self.failures - 1).min(5)).min(RETRY_MAX))
    }

    /// The pause is over: ask again, unless the service has gone meanwhile.
    fn retry(&mut self) -> Vec<Update> {
        if self.client.present() == Some(false) {
            return self.absent(ClientError::NotRunning);
        }
        self.handle(Job::Refresh)
    }

    fn handle(&mut self, job: Job) -> Vec<Update> {
        match job {
            Job::Refresh => match self.client.get_state() {
                Ok(s) => self.state(s),
                Err(e) => self.absent(e),
            },
            Job::Changed(Change::State(s)) => self.state(s),
            Job::Changed(Change::Appeared) => self.handle(Job::Refresh),
            Job::Changed(Change::Vanished) => self.absent(ClientError::NotRunning),
        }
    }
}

/// One call; a service found gone is followed up by asking for the state.
fn call(client: &dyn Client, request: Request) -> (Update, bool) {
    let result = client.call(&request);
    let gone = result == Err(ClientError::NotRunning);
    (Update::Replied { request, result }, gone)
}

enum Mode {
    Thread {
        jobs: Sender<Job>,
        calls: Sender<Request>,
        updates: Receiver<Update>,
    },
    Inline {
        follower: Follower,
        jobs: Arc<Mutex<VecDeque<Job>>>,
        calls: VecDeque<Request>,
        started: bool,
    },
}

pub struct Link {
    mode: Mode,
}

impl Link {
    /// The client on threads of its own; `wake` is called after every update (the
    /// window's repaint).
    pub fn spawn(client: Box<dyn Client>, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let client: Arc<dyn Client> = Arc::from(client);
        let wake = Arc::new(wake);
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (calls, call_rx) = mpsc::channel::<Request>();
        let (update_tx, updates) = mpsc::channel();

        let send = {
            let (tx, wake) = (update_tx.clone(), wake.clone());
            move |out: Vec<Update>| {
                let ok = out.into_iter().all(|u| tx.send(u).is_ok());
                wake();
                ok
            }
        };
        let changes = jobs.clone();
        let mut follower = Follower::new(client.clone());
        std::thread::Builder::new()
            .name("api-state".into())
            .spawn(move || {
                let sink: Sink = Box::new(move |c| {
                    let _ = changes.send(Job::Changed(c));
                });
                if !send(follower.start(sink)) {
                    return;
                }
                loop {
                    let next = match follower.retry_in() {
                        Some(pause) => job_rx.recv_timeout(pause),
                        None => job_rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    };
                    let out = match next {
                        Ok(job) => follower.handle(job),
                        Err(RecvTimeoutError::Timeout) => follower.retry(),
                        Err(RecvTimeoutError::Disconnected) => return,
                    };
                    if !send(out) {
                        return;
                    }
                }
            })
            .expect("a thread for the API's state");

        let refresh = jobs.clone();
        std::thread::Builder::new()
            .name("api-calls".into())
            .spawn(move || {
                while let Ok(request) = call_rx.recv() {
                    let (update, gone) = call(&*client, request);
                    if update_tx.send(update).is_err() {
                        return;
                    }
                    wake();
                    if gone {
                        let _ = refresh.send(Job::Refresh);
                    }
                }
            })
            .expect("a thread for the API's calls");
        Self {
            mode: Mode::Thread {
                jobs,
                calls,
                updates,
            },
        }
    }

    /// The client on the caller's thread: everything runs inside [`Link::poll`].
    pub fn inline(client: Box<dyn Client>) -> Self {
        Self {
            mode: Mode::Inline {
                follower: Follower::new(Arc::from(client)),
                jobs: Arc::new(Mutex::new(VecDeque::new())),
                calls: VecDeque::new(),
                started: false,
            },
        }
    }

    pub fn send(&mut self, request: Request) {
        match &mut self.mode {
            Mode::Thread { calls, .. } => {
                let _ = calls.send(request);
            }
            Mode::Inline { calls, .. } => calls.push_back(request),
        }
    }

    /// Ask for the whole state again.
    pub fn refresh(&self) {
        match &self.mode {
            Mode::Thread { jobs, .. } => {
                let _ = jobs.send(Job::Refresh);
            }
            Mode::Inline { jobs, .. } => jobs.lock().unwrap().push_back(Job::Refresh),
        }
    }

    /// Everything learnt since the last poll.
    pub fn poll(&mut self) -> Vec<Update> {
        match &mut self.mode {
            Mode::Thread { updates, .. } => updates.try_iter().collect(),
            Mode::Inline {
                follower,
                jobs,
                calls,
                started,
            } => {
                let mut out = Vec::new();
                if !*started {
                    *started = true;
                    let queue = jobs.clone();
                    out.extend(follower.start(Box::new(move |c| {
                        queue.lock().unwrap().push_back(Job::Changed(c));
                    })));
                }
                while let Some(request) = calls.pop_front() {
                    let (update, gone) = call(&*follower.client, request);
                    out.push(update);
                    if gone {
                        jobs.lock().unwrap().push_back(Job::Refresh);
                    }
                }
                loop {
                    let next = jobs.lock().unwrap().pop_front();
                    let Some(job) = next else { break };
                    out.extend(follower.handle(job));
                }
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::Mock;
    use std::time::Instant;

    fn collect(link: &mut Link, until: impl Fn(&[Update]) -> bool) -> Vec<Update> {
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !until(&got) && Instant::now() < deadline {
            got.extend(link.poll());
            std::thread::sleep(Duration::from_millis(5));
        }
        got
    }

    #[test]
    fn inline_gets_the_state_then_replies_and_changes() {
        let mut link = Link::inline(Box::new(Mock::connected()));
        let first = link.poll();
        assert!(matches!(&first[..], [Update::State(s)] if s.state.displays.len() == 4));
        link.send(Request::RemoveMonitor { id: 1 });
        let next = link.poll();
        assert!(
            matches!(&next[..], [Update::Replied { result: Ok(_), .. }, Update::State(s)] if s.state.displays.len() == 3),
            "{next:?}"
        );
    }

    #[test]
    fn a_missing_service_is_absent_until_it_appears() {
        let mock = Mock::not_running();
        let handle = mock.handle();
        let mut link = Link::inline(Box::new(mock));
        assert_eq!(link.poll(), [Update::Absent(ClientError::NotRunning)]);
        handle.set_running(true);
        assert!(matches!(&link.poll()[..], [Update::State(_)]));
        handle.set_running(false);
        assert_eq!(link.poll(), [Update::Absent(ClientError::NotRunning)]);
    }

    #[test]
    fn an_older_state_never_replaces_a_newer_one() {
        let mut f = Follower::new(Arc::new(Mock::connected()));
        let s = |revision| Snapshot {
            revision,
            ..Default::default()
        };
        assert_eq!(f.handle(Job::Changed(Change::State(s(5)))).len(), 1);
        assert!(f.handle(Job::Changed(Change::State(s(4)))).is_empty());
        assert_eq!(f.handle(Job::Changed(Change::State(s(5)))).len(), 1);
        f.handle(Job::Changed(Change::Vanished));
        assert_eq!(f.handle(Job::Changed(Change::State(s(1)))).len(), 1);
    }

    #[test]
    fn a_service_that_does_not_answer_is_asked_again_ever_later() {
        let mut f = Follower::new(Arc::new(Mock::connected()));
        assert_eq!(f.retry_in(), None);
        f.absent(ClientError::TimedOut);
        assert_eq!(f.retry_in(), Some(Duration::from_secs(1)));
        f.absent(ClientError::Broken("?".into()));
        assert_eq!(f.retry_in(), Some(Duration::from_secs(2)));
        for _ in 0..10 {
            f.absent(ClientError::TimedOut);
        }
        assert_eq!(f.retry_in(), Some(RETRY_MAX));
        // It answers: no more tries.
        assert!(matches!(&f.retry()[..], [Update::State(_)]));
        assert_eq!(f.retry_in(), None);
        // Gone: the bus says when it is back, nothing is asked meanwhile.
        f.absent(ClientError::TimedOut);
        f.absent(ClientError::NotRunning);
        assert_eq!(f.retry_in(), None);
    }

    #[test]
    fn the_threads_wake_the_window() {
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = woken.clone();
        let mut link = Link::spawn(Box::new(Mock::connected()), move || {
            w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        link.send(Request::SetPrimary { id: 3 });
        // The call and the first state run on their own threads: either may come
        // first, and the state the window ends with has the change.
        let changed = |u: &Update| matches!(u, Update::State(s) if s.state.primary == Some(3));
        let got = collect(&mut link, |g| {
            g.iter().any(changed) && g.iter().any(|u| matches!(u, Update::Replied { .. }))
        });
        assert!(got.iter().any(changed), "{got:?}");
        assert!(
            got.iter()
                .any(|u| matches!(u, Update::Replied { result: Ok(_), .. }))
        );
        assert!(woken.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }

    /// A service whose calls wait until the test lets them go.
    struct Slow {
        sink: Mutex<Option<Sink>>,
        release: Mutex<Receiver<()>>,
    }

    impl Client for Slow {
        fn get_state(&self) -> Result<Snapshot, ClientError> {
            Ok(Snapshot::default())
        }

        fn call(&self, _: &Request) -> Result<Answer, ClientError> {
            let _ = self.release.lock().unwrap().recv();
            Ok(Answer::default())
        }

        fn watch(&self, sink: Sink) -> Result<(), ClientError> {
            *self.sink.lock().unwrap() = Some(sink);
            Ok(())
        }
    }

    #[test]
    fn a_slow_call_holds_up_no_change() {
        let (release, rx) = mpsc::channel();
        let slow = Arc::new(Slow {
            sink: Mutex::new(None),
            release: Mutex::new(rx),
        });
        struct Shared(Arc<Slow>);
        impl Client for Shared {
            fn get_state(&self) -> Result<Snapshot, ClientError> {
                self.0.get_state()
            }
            fn call(&self, r: &Request) -> Result<Answer, ClientError> {
                self.0.call(r)
            }
            fn watch(&self, sink: Sink) -> Result<(), ClientError> {
                self.0.watch(sink)
            }
        }
        let mut link = Link::spawn(Box::new(Shared(slow.clone())), || {});
        collect(&mut link, |g| !g.is_empty());
        link.send(Request::StopPairing);
        std::thread::sleep(Duration::from_millis(50));
        // The call waits; a change comes meanwhile and reaches the window at once.
        let changed = Snapshot {
            revision: 7,
            ..Default::default()
        };
        (slow.sink.lock().unwrap().as_ref().unwrap())(Change::State(changed));
        let got = collect(&mut link, |g| !g.is_empty());
        assert!(
            matches!(&got[..], [Update::State(s)] if s.revision == 7),
            "{got:?}"
        );
        release.send(()).unwrap();
        let got = collect(&mut link, |g| !g.is_empty());
        assert!(
            matches!(&got[..], [Update::Replied { result: Ok(_), .. }]),
            "{got:?}"
        );
    }
}
