//! One settings window per person: the window owns
//! `io.github.OlexiyOdarchuk.Prostir.Settings` on the session bus, and a second start
//! asks the first one to come forward (`Present`) and quits. The service also sees the
//! window by this name (it keeps the service running while the window is open).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;
use zbus::blocking::Connection;
use zbus::fdo::{RequestNameFlags, RequestNameReply};

pub const NAME: &str = "io.github.OlexiyOdarchuk.Prostir.Settings";
pub const PATH: &str = "/io/github/OlexiyOdarchuk/Prostir/Settings";
pub const INTERFACE: &str = "io.github.OlexiyOdarchuk.Prostir.Settings1";

/// Set when someone asked the window to come forward; the window clears it.
#[derive(Clone, Default)]
pub struct Presented {
    asked: Arc<AtomicBool>,
    ctx: Arc<Mutex<Option<egui::Context>>>,
}

impl Presented {
    /// The window's context, to wake it when asked.
    pub fn attach(&self, ctx: &egui::Context) {
        *self.ctx.lock().unwrap() = Some(ctx.clone());
    }

    /// Whether the window was asked to come forward since the last look.
    pub fn take(&self) -> bool {
        self.asked.swap(false, Ordering::Relaxed)
    }

    fn ask(&self) {
        self.asked.store(true, Ordering::Relaxed);
        if let Some(ctx) = &*self.ctx.lock().unwrap() {
            ctx.request_repaint();
        }
    }
}

struct Window {
    presented: Presented,
}

#[zbus::interface(name = "io.github.OlexiyOdarchuk.Prostir.Settings1")]
impl Window {
    /// Brings the window forward.
    fn present(&self) {
        self.presented.ask();
    }
}

pub enum Start {
    /// This is the window; it comes forward when `Presented` says so.
    First(Presented),
    /// Another window was there and has been asked to come forward.
    Second,
}

/// Claims the name, or hands over to the window that has it.
pub fn claim(conn: &Connection) -> zbus::Result<Start> {
    let presented = Presented::default();
    // The object first, so a window found by its name always answers.
    conn.object_server().at(
        PATH,
        Window {
            presented: presented.clone(),
        },
    )?;
    // zbus reports a name someone else has as an error rather than as `Exists`.
    match conn.request_name_with_flags(NAME, RequestNameFlags::DoNotQueue.into()) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
            Ok(Start::First(presented))
        }
        Ok(RequestNameReply::Exists | RequestNameReply::InQueue) | Err(zbus::Error::NameTaken) => {
            let _ = conn.object_server().remove::<Window, _>(PATH);
            conn.call_method(Some(NAME), PATH, Some(INTERFACE), "Present", &())?;
            Ok(Start::Second)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_window_brings_the_first_forward() {
        let Some(bus) = crate::dbus::tests::bus() else {
            return;
        };
        let first = crate::dbus::tests::connect(&bus);
        let Start::First(presented) = claim(&first).unwrap() else {
            panic!("the first window owns the name");
        };
        assert!(!presented.take());
        let second = crate::dbus::tests::connect(&bus);
        assert!(matches!(claim(&second).unwrap(), Start::Second));
        assert!(presented.take(), "asked to come forward");
        assert!(!presented.take());
    }
}
