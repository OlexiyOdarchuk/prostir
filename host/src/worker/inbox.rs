//! What the control loop hands the desktop's worker: the wanted state, of which only
//! the newest counts (a newer one replaces one the worker has not started on), and
//! one-off jobs, kept in order (a job like one already waiting joins it). The worker
//! sleeps in [`Inbox::take`] until something comes, or until a time of its own.

use std::sync::{Condvar, Mutex};
use std::time::Instant;

/// A job that can join one of its kind already waiting.
pub trait Mergeable {
    /// Takes `other` into `self` if they are of one kind (`other` is then dropped);
    /// whether it did.
    fn merge(&mut self, other: &Self) -> bool;
}

/// What waits for the worker.
#[derive(Debug)]
pub struct Mail<W, J> {
    /// The newest wanted state, if one came since the last take.
    pub wanted: Option<W>,
    pub jobs: Vec<J>,
    /// The worker is to put everything back and end.
    pub quit: bool,
    /// How many things were handed over up to this mail (see [`Inbox::want`]).
    pub upto: u64,
}

impl<W, J> Default for Mail<W, J> {
    fn default() -> Self {
        Self {
            wanted: None,
            jobs: Vec::new(),
            quit: false,
            upto: 0,
        }
    }
}

impl<W, J> Mail<W, J> {
    fn is_empty(&self) -> bool {
        self.wanted.is_none() && self.jobs.is_empty() && !self.quit
    }
}

/// The worker's inbox, shared by the control loop (which fills it) and the worker.
pub struct Inbox<W, J> {
    mail: Mutex<Mail<W, J>>,
    ring: Condvar,
}

impl<W, J: Mergeable> Default for Inbox<W, J> {
    fn default() -> Self {
        Self {
            mail: Mutex::new(Mail::default()),
            ring: Condvar::new(),
        }
    }
}

impl<W, J: Mergeable> Inbox<W, J> {
    /// The state wanted now: it replaces one the worker has not taken yet. Returns how
    /// many things were handed over so far, this one too: the worker says how many it
    /// took ([`Mail::upto`]), so whoever waits for it knows whether it has all.
    pub fn want(&self, wanted: W) -> u64 {
        let mut mail = self.mail.lock().unwrap();
        mail.wanted = Some(wanted);
        mail.upto += 1;
        let upto = mail.upto;
        drop(mail);
        self.ring.notify_one();
        upto
    }

    /// A job, after those waiting (or into one of its kind waiting); returns as
    /// [`Inbox::want`].
    pub fn ask(&self, job: J) -> u64 {
        let mut mail = self.mail.lock().unwrap();
        if !mail.jobs.iter_mut().any(|j| j.merge(&job)) {
            mail.jobs.push(job);
        }
        mail.upto += 1;
        let upto = mail.upto;
        drop(mail);
        self.ring.notify_one();
        upto
    }

    /// Asks the worker to end.
    pub fn quit(&self) {
        self.mail.lock().unwrap().quit = true;
        self.ring.notify_one();
    }

    /// Everything waiting, once something does or `until` passes (`None`: no time of
    /// its own, so it sleeps until something comes).
    pub fn take(&self, until: Option<Instant>) -> Mail<W, J> {
        let mut mail = self.mail.lock().unwrap();
        while mail.is_empty() {
            match until {
                None => mail = self.ring.wait(mail).unwrap(),
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    mail = self.ring.wait_timeout(mail, left).unwrap().0;
                }
            }
        }
        let upto = mail.upto;
        let mut taken = std::mem::take(&mut *mail);
        // The count goes on.
        mail.upto = upto;
        taken.upto = upto;
        taken
    }

    /// What came while the worker was busy, without waiting: a newer wanted state, the
    /// jobs, whether to quit (which stays asked for the next take), and how many things
    /// were handed over up to them.
    pub fn peek_newer(&self) -> (Option<W>, Vec<J>, bool, u64) {
        let mut mail = self.mail.lock().unwrap();
        (
            mail.wanted.take(),
            std::mem::take(&mut mail.jobs),
            mail.quit,
            mail.upto,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Debug, PartialEq)]
    enum Job {
        Tick,
        Fresh(Vec<u32>),
        Shot(u32),
    }

    impl Mergeable for Job {
        fn merge(&mut self, other: &Self) -> bool {
            match (self, other) {
                (Job::Tick, Job::Tick) => true,
                (Job::Fresh(a), Job::Fresh(b)) => {
                    a.extend(b.iter().filter(|id| !a.contains(id)).collect::<Vec<_>>());
                    true
                }
                _ => false,
            }
        }
    }

    /// Wanted states the worker did not start on are replaced by the newest: three
    /// changes while it was busy make one piece of work, the last.
    #[test]
    fn the_newest_wanted_state_wins() {
        let inbox: Inbox<u32, Job> = Inbox::default();
        for w in 1..=3 {
            inbox.want(w);
        }
        let mail = inbox.take(None);
        assert_eq!(mail.wanted, Some(3));
        assert!(mail.jobs.is_empty() && !mail.quit);
        // Taken: the next take waits for something new.
        let mail = inbox.take(Some(Instant::now() + Duration::from_millis(20)));
        assert_eq!(mail.wanted, None);
    }

    /// While the worker works on one state, a newer one is seen between its steps.
    #[test]
    fn a_newer_state_is_seen_between_steps() {
        let inbox: Inbox<u32, Job> = Inbox::default();
        inbox.want(1);
        assert_eq!(inbox.take(None).wanted, Some(1));
        assert_eq!(inbox.peek_newer(), (None, Vec::new(), false, 1));
        inbox.want(2);
        assert_eq!(inbox.want(3), 3);
        let (newer, jobs, quit, upto) = inbox.peek_newer();
        assert_eq!(
            (newer, jobs.is_empty(), quit, upto),
            (Some(3), true, false, 3)
        );
    }

    /// Jobs keep their order and are never replaced; those of one kind join.
    #[test]
    fn jobs_keep_their_order_and_like_ones_join() {
        let inbox: Inbox<u32, Job> = Inbox::default();
        inbox.ask(Job::Tick);
        inbox.ask(Job::Shot(1));
        inbox.ask(Job::Fresh(vec![1]));
        inbox.ask(Job::Tick);
        inbox.ask(Job::Fresh(vec![2, 1]));
        inbox.ask(Job::Shot(2));
        inbox.want(7);
        let mail = inbox.take(None);
        assert_eq!(
            mail.jobs,
            [
                Job::Tick,
                Job::Shot(1),
                Job::Fresh(vec![1, 2]),
                Job::Shot(2)
            ]
        );
        assert_eq!(mail.wanted, Some(7));
        // Each counts, joined or not: the worker that took them took all seven.
        assert_eq!(mail.upto, 7);
        assert_eq!(inbox.ask(Job::Tick), 8);
    }

    /// The worker sleeps until something comes; with a time of its own, until then.
    #[test]
    fn the_worker_sleeps_until_work_comes() {
        let inbox: Arc<Inbox<u32, Job>> = Arc::new(Inbox::default());
        let started = Instant::now();
        let mail = inbox.take(Some(started + Duration::from_millis(50)));
        assert!(mail.wanted.is_none() && mail.jobs.is_empty());
        assert!(started.elapsed() >= Duration::from_millis(50));

        let sender = inbox.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            sender.want(9);
        });
        let started = Instant::now();
        assert_eq!(inbox.take(None).wanted, Some(9));
        assert!(started.elapsed() >= Duration::from_millis(40));
        t.join().unwrap();

        inbox.quit();
        assert!(inbox.take(None).quit);
    }
}
