//! The control loop's side of the desktop's worker (`worker/`): what the worker says,
//! sessions that wait for it to connect, requests that wait for its answer, and the
//! watch on it. Nothing here waits for the worker.

use std::time::Instant;

use log::{error, info, warn};
use prostir_proto::HostMsg;
use prostir_proto::state::{ErrorCode, MAX_DETAIL, clip};

use crate::brain::Phase;
use crate::displays::DeskNews;
use crate::events::Accepted;
use crate::session::{Session, desktop_not_responding};
use crate::worker::Done;
use crate::worker::watch::Change;
use crate::{Brain, keep_screens, requests, turn_away};

impl Brain {
    /// What the desktop's worker `generation` said.
    pub(crate) fn desk(&mut self, generation: u64, done: Done) {
        let Some(displays) = self.displays.as_mut() else {
            return;
        };
        // A worker let go says no more of the screens.
        if displays.desk().generation() != generation {
            return;
        }
        let store = &mut self.store;
        let news = displays.desk_done(done, &mut || store.new_id());
        // A display of the PC seen for the first time is kept.
        keep_screens(&mut self.store, displays, false);
        match news {
            DeskNews::None => {}
            DeskNews::Connected => {
                // The sessions that waited for the desktop open now.
                for accepted in std::mem::take(&mut self.waiting_desk) {
                    self.open_session(accepted);
                }
            }
            DeskNews::NoDesktop(e) => {
                error!("{e:#}");
                // Tell the headsets why, instead of a snapshot.
                for accepted in std::mem::take(&mut self.waiting_desk) {
                    turn_away(accepted.conn, format!("{e:#}"));
                }
                if self.sessions.is_empty() {
                    self.displays = None;
                    self.phase = Phase::Idle;
                }
            }
            DeskNews::Job(job, result, answer) => {
                let outcome = requests::outcome_of_job(result, answer);
                for s in self.sessions.values_mut() {
                    if s.job_done(job, &outcome) {
                        break;
                    }
                }
            }
        }
        self.look_at_desk(Instant::now());
    }

    /// A connection waits for the desktop's worker to connect before its session
    /// opens (the worker is started for it if there is none).
    pub(crate) fn wait_for_desk(&mut self, accepted: Accepted) {
        self.waiting_desk.push(accepted);
    }

    /// Looks at the desktop's worker at `now`: stuck for 15 s, the desktop does not
    /// respond (the tray and the local API show it, the headsets are told, requests
    /// waiting for it fail); back, the PC is fine again; stuck for 2 min, the program
    /// ends, so that whoever started it starts it again.
    pub(crate) fn look_at_desk(&mut self, now: Instant) {
        let Some(displays) = self.displays.as_mut() else {
            return;
        };
        let step = displays.desk().step();
        let (stuck_after, give_up_after) = displays.desk().dog.limits();
        match displays.desk_mut().dog.look(step, now) {
            Change::None => {}
            Change::Stuck(what) => {
                warn!(
                    "the desktop does not respond: {what} has taken more than {} s",
                    stuck_after.as_secs()
                );
                displays.router.send_all(HostMsg::Problem {
                    error: ErrorCode::DESKTOP,
                    detail: clip(&desktop_not_responding(), MAX_DETAIL),
                    display: None,
                });
                for s in self.sessions.values_mut() {
                    s.give_up_waiting(now, true);
                }
                for accepted in std::mem::take(&mut self.waiting_desk) {
                    turn_away(accepted.conn, desktop_not_responding());
                }
            }
            Change::Back => info!("the desktop responds again"),
            Change::GiveUp(what) => {
                error!(
                    "the desktop has not responded for {} s ({what}): quitting, to be started again",
                    give_up_after.as_secs()
                );
                self.desk_gave_up = true;
                self.quit = true;
            }
        }
    }

    /// When the control loop has to look at the desktop's worker or at requests
    /// waiting for it, if ever.
    pub(crate) fn desk_due(&self) -> Option<Instant> {
        let now = Instant::now();
        let dog = self
            .displays
            .as_ref()
            .and_then(|d| d.desk().dog.due(d.desk().step().map(|(t, _)| t), now));
        let answers = self
            .sessions
            .values()
            .filter_map(Session::waiting_until)
            .min();
        crate::brain::earliest([dog, answers])
    }

    /// Requests that waited for the desktop long enough fail.
    pub(crate) fn desk_answers_due(&mut self, now: Instant) {
        for s in self.sessions.values_mut() {
            s.give_up_waiting(now, false);
        }
    }
}
