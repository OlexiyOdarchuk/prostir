//! The connection to the headset: socket options, which depend on the operating
//! system and not on the desktop (the fake desktop of the tests uses them too).

use std::net::TcpStream;

use log::warn;
use prostir_proto::link::tcp::SocketOptions;

/// The options of a connection to the headset.
pub struct TcpOptions;

#[cfg(target_os = "linux")]
fn set(stream: &TcpStream, level: libc::c_int, name: libc::c_int, value: u32) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: a valid socket and a 4-byte option value that lives through the call.
    unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            level,
            name,
            (&value as *const u32).cast(),
            std::mem::size_of::<u32>() as libc::socklen_t,
        ) == 0
    }
}

#[cfg(target_os = "linux")]
impl SocketOptions for TcpOptions {
    /// Notices a headset that vanished without closing the connection (switched off,
    /// out of Wi-Fi) within about 10 s, also while pairing, when the session's own
    /// silence check does not run yet; keeps little unsent data in the kernel.
    fn prepare(&self, stream: &TcpStream) {
        let ok = set(stream, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1)
            && set(stream, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 4)
            && set(stream, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 2)
            && set(stream, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3)
            // Unacknowledged data (frames) also gives up after 10 s.
            && set(stream, libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, 10_000)
            // Linux would let unsent data grow to megabytes: frames wait in the
            // outbox instead, where late ones can still be dropped.
            && set(stream, libc::IPPROTO_TCP, libc::TCP_NOTSENT_LOWAT, 256 * 1024);
        if !ok {
            warn!(
                "cannot set up the connection's socket: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    /// TCP spreads its packets out to this rate itself (no queueing discipline
    /// needed).
    fn set_pace(&self, stream: &TcpStream, bytes_per_sec: u64) {
        let rate = match bytes_per_sec {
            0 => u32::MAX, // what the kernel reads as no limit
            n => u32::try_from(n).unwrap_or(u32::MAX - 1),
        };
        if !set(stream, libc::SOL_SOCKET, libc::SO_MAX_PACING_RATE, rate) {
            warn!(
                "cannot pace the connection: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}
