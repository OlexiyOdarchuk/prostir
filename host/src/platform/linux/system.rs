//! Linux system bits that do not depend on the desktop.

/// Whether the laptop lid is open (true on machines without a lid).
pub fn lid_open() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc/acpi/button/lid") else {
        return true;
    };
    entries
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("state")).ok())
        .all(|state| !state.contains("closed"))
}
