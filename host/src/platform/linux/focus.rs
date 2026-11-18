//! Which display the user works on: KWin's active output (the one with the mouse,
//! or with the focused window, depending on the KDE setting), or Hyprland's focused
//! monitor.

use zbus::blocking::Connection;

pub struct ActiveOutput {
    bus: Option<Connection>,
}

impl ActiveOutput {
    pub fn new() -> Self {
        Self {
            bus: Connection::session().ok(),
        }
    }

    /// The output name, e.g. `Virtual-Prostir-1` or `eDP-1`.
    pub fn get(&self) -> Option<String> {
        if crate::platform::hyprland::running() {
            return crate::platform::hyprland::focused();
        }
        let reply = self
            .bus
            .as_ref()?
            .call_method(
                Some("org.kde.KWin"),
                "/KWin",
                Some("org.kde.KWin"),
                "activeOutputName",
                &(),
            )
            .ok()?;
        reply.body().deserialize::<String>().ok()
    }
}
