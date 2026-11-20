//! The window's line to the service: one thread owns the client, so a slow or hung
//! service never stops the window from drawing. The window sends requests and reads
//! updates; the thread asks for the state again whenever the service appears.
//!
//! In tests the same logic runs on the window's thread instead (`Link::inline`), so a
//! picture is taken of exactly the state that was asked for.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::api::{Change, Client, ClientError, Reply, Request, Snapshot};

/// What the window learns.
#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    /// A state newer than any before.
    State(Snapshot),
    /// The service is not there (or cannot be reached, and why).
    Absent(ClientError),
    /// A request was answered.
    Replied {
        request: Request,
        result: Result<Reply, ClientError>,
    },
}

enum Job {
    Refresh,
    Call(Request),
    Changed(Change),
}

/// The logic, the same on the thread and inline.
struct Driver {
    client: Box<dyn Client>,
    revision: Option<u32>,
}

impl Driver {
    fn start(&mut self, sink: crate::api::Sink) -> Vec<Update> {
        if let Err(e) = self.client.watch(sink) {
            return vec![Update::Absent(e)];
        }
        self.handle(Job::Refresh)
    }

    fn state(&mut self, s: Snapshot) -> Vec<Update> {
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
        vec![Update::Absent(e)]
    }

    fn handle(&mut self, job: Job) -> Vec<Update> {
        match job {
            Job::Refresh => match self.client.get_state() {
                Ok(s) => self.state(s),
                Err(e) => self.absent(e),
            },
            Job::Call(request) => {
                let result = self.client.call(&request);
                let mut out = Vec::new();
                if result == Err(ClientError::NotRunning) {
                    out.extend(self.absent(ClientError::NotRunning));
                }
                out.push(Update::Replied { request, result });
                out
            }
            Job::Changed(Change::State(s)) => self.state(s),
            Job::Changed(Change::Appeared) => self.handle(Job::Refresh),
            Job::Changed(Change::Vanished) => self.absent(ClientError::NotRunning),
        }
    }
}

enum Mode {
    Thread {
        jobs: Sender<Job>,
        updates: Receiver<Update>,
    },
    Inline {
        driver: Driver,
        jobs: Arc<Mutex<VecDeque<Job>>>,
        started: bool,
    },
}

pub struct Link {
    mode: Mode,
}

impl Link {
    /// The client on its own thread; `wake` is called after every update (the
    /// window's repaint).
    pub fn spawn(client: Box<dyn Client>, wake: impl Fn() + Send + 'static) -> Self {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (update_tx, updates) = mpsc::channel();
        let changes = jobs.clone();
        std::thread::Builder::new()
            .name("api".into())
            .spawn(move || {
                let mut driver = Driver {
                    client,
                    revision: None,
                };
                let send = |out: Vec<Update>| {
                    for u in out {
                        if update_tx.send(u).is_err() {
                            return false;
                        }
                    }
                    wake();
                    true
                };
                let sink: crate::api::Sink = Box::new(move |c| {
                    let _ = changes.send(Job::Changed(c));
                });
                if !send(driver.start(sink)) {
                    return;
                }
                while let Ok(job) = job_rx.recv() {
                    if !send(driver.handle(job)) {
                        return;
                    }
                }
            })
            .expect("a thread for the API");
        Self {
            mode: Mode::Thread { jobs, updates },
        }
    }

    /// The client on the caller's thread: every job runs inside [`Link::poll`].
    pub fn inline(client: Box<dyn Client>) -> Self {
        Self {
            mode: Mode::Inline {
                driver: Driver {
                    client,
                    revision: None,
                },
                jobs: Arc::new(Mutex::new(VecDeque::new())),
                started: false,
            },
        }
    }

    fn push(&self, job: Job) {
        match &self.mode {
            Mode::Thread { jobs, .. } => {
                let _ = jobs.send(job);
            }
            Mode::Inline { jobs, .. } => jobs.lock().unwrap().push_back(job),
        }
    }

    pub fn send(&self, request: Request) {
        self.push(Job::Call(request));
    }

    /// Ask for the whole state again.
    pub fn refresh(&self) {
        self.push(Job::Refresh);
    }

    /// Everything learnt since the last poll.
    pub fn poll(&mut self) -> Vec<Update> {
        match &mut self.mode {
            Mode::Thread { updates, .. } => updates.try_iter().collect(),
            Mode::Inline {
                driver,
                jobs,
                started,
            } => {
                let mut out = Vec::new();
                if !*started {
                    *started = true;
                    let queue = jobs.clone();
                    out.extend(driver.start(Box::new(move |c| {
                        queue.lock().unwrap().push_back(Job::Changed(c));
                    })));
                }
                loop {
                    let next = jobs.lock().unwrap().pop_front();
                    let Some(job) = next else { break };
                    out.extend(driver.handle(job));
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
        let mut d = Driver {
            client: Box::new(Mock::connected()),
            revision: None,
        };
        let s = |revision| Snapshot {
            revision,
            ..Default::default()
        };
        assert_eq!(d.handle(Job::Changed(Change::State(s(5)))).len(), 1);
        assert!(d.handle(Job::Changed(Change::State(s(4)))).is_empty());
        assert_eq!(d.handle(Job::Changed(Change::State(s(5)))).len(), 1);
        d.handle(Job::Changed(Change::Vanished));
        assert_eq!(d.handle(Job::Changed(Change::State(s(1)))).len(), 1);
    }

    #[test]
    fn the_thread_wakes_the_window() {
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = woken.clone();
        let mut link = Link::spawn(Box::new(Mock::connected()), move || {
            w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        link.send(Request::SetPrimary { id: 3 });
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while got.len() < 3 && std::time::Instant::now() < deadline {
            got.extend(link.poll());
            std::thread::yield_now();
        }
        assert!(matches!(got[0], Update::State(_)));
        assert!(
            got.iter()
                .any(|u| matches!(u, Update::Replied { result: Ok(_), .. }))
        );
        assert!(woken.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }
}
