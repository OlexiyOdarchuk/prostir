//! Signals that ask Prostir to quit (SIGTERM, SIGINT, SIGHUP): a handler writes the
//! signal's number to a pipe, and a thread of its own sleeps in the kernel reading it
//! until one comes. The same on every Linux desktop and on the fake one.
//!
//! Not a signalfd: that needs the signals blocked in every thread, and programs started
//! with `std::process::Command` keep a blocked mask, so a helper stopped with SIGTERM
//! (pw-loopback when the session ends, say) would not stop. A handler is reset in a
//! started program.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use log::{info, warn};

/// The signals that ask Prostir to quit.
const QUIT_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// How long quitting may take once asked by a signal: stuck past it (in a dialog, say),
/// the monitors are removed directly and the program leaves.
const QUIT_WITHIN: Duration = Duration::from_secs(5);

/// The end of the pipe the handler writes to (-1: none yet).
static PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(signal: libc::c_int) {
    let byte = signal as u8;
    // SAFETY: only async-signal-safe calls (`write`), on a descriptor that stays open
    // for the life of the process; errno is put back as the interrupted code had it.
    unsafe {
        let errno = *libc::__errno_location();
        libc::write(PIPE.load(Ordering::Relaxed), (&raw const byte).cast(), 1);
        *libc::__errno_location() = errno;
    }
}

/// The signal's name, for the log.
fn name(signal: u8) -> &'static str {
    match libc::c_int::from(signal) {
        libc::SIGTERM => "SIGTERM",
        libc::SIGINT => "SIGINT",
        libc::SIGHUP => "SIGHUP",
        _ => "a signal",
    }
}

/// The pipe, and the handler for the quit signals: the end to read them from.
fn install() -> std::io::Result<OwnedFd> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 fills the two descriptors, owned right below.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: new descriptors nothing else owns.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // A burst of signals never blocks the handler: what does not fit is dropped (one
    // is enough).
    // SAFETY: plain fcntl on a valid descriptor.
    unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    // The handler writes to it for the life of the process.
    PIPE.store(
        std::os::fd::IntoRawFd::into_raw_fd(write),
        Ordering::Relaxed,
    );
    for signal in QUIT_SIGNALS {
        // SAFETY: an all-zero sigaction is valid; the handler only does what a signal
        // handler may (see `on_signal`).
        let failed = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut()) != 0
        };
        if failed {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(read)
}

/// Sleeps until a quit signal comes; its name (`None`: the pipe broke).
fn wait_for_signal(pipe: &OwnedFd) -> Option<&'static str> {
    let mut byte = 0u8;
    loop {
        // SAFETY: reads one byte into a byte.
        let n = unsafe { libc::read(pipe.as_raw_fd(), (&raw mut byte).cast(), 1) };
        match n {
            1 => return Some(name(byte)),
            n if n < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            _ => return None,
        }
    }
}

/// Lets the quit signals end the program cleanly: `on_quit` is called with the
/// signal's name on a thread of its own, once. If the normal way out does not finish
/// within a few seconds, the program removes its monitors (what KWin would do itself,
/// Hyprland not) and leaves.
pub fn quit_on_signals(on_quit: impl FnOnce(&'static str) + Send + 'static) {
    let pipe = match install() {
        Ok(pipe) => pipe,
        Err(e) => {
            warn!("cannot watch for signals: {e}");
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let Some(signal) = wait_for_signal(&pipe) else {
                warn!("reading signals failed: quit signals are ignored");
                return;
            };
            info!("{signal} received: quitting");
            on_quit(signal);
            std::thread::sleep(QUIT_WITHIN);
            warn!(
                "did not quit within {} s: removing monitors and leaving",
                QUIT_WITHIN.as_secs()
            );
            crate::platform::clean_up_after_crash();
            std::process::exit(1);
        });
    if let Err(e) = spawned {
        warn!("cannot watch for signals: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quit signal reaches the thread that waits for it, and a program started
    /// afterwards has none of them blocked or caught: stopping a helper (pw-loopback,
    /// say) with SIGTERM still stops it.
    #[test]
    fn a_signal_reaches_the_pipe_and_programs_keep_theirs() {
        let pipe = install().unwrap();
        // SAFETY: raises a signal whose handler is installed just above.
        assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
        assert_eq!(wait_for_signal(&pipe), Some("SIGHUP"));
        let out = std::process::Command::new("sh")
            .args(["-c", "grep -E 'SigBlk|SigCgt' /proc/self/status"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let mask = u64::from_str_radix(line.split_whitespace().nth(1).unwrap(), 16).unwrap();
            for signal in QUIT_SIGNALS {
                assert_eq!(mask & (1 << (signal - 1)), 0, "{line}");
            }
        }
        assert_eq!(text.lines().count(), 2, "{text}");
    }
}
