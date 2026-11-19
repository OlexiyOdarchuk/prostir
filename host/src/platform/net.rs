//! The connection to the headset: socket options, which depend on the operating
//! system and not on the desktop (the fake desktop of the tests uses them too).

use std::net::TcpStream;

/// Notices a headset that vanished without closing the connection (switched off,
/// out of Wi-Fi) within about 10 s, instead of waiting forever.
#[cfg(target_os = "linux")]
pub fn set_keepalive(stream: &TcpStream) {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let set = |level, name, value: libc::c_int| {
        // SAFETY: a valid socket and a c_int option value.
        unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                (&value as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 4);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 2);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
    // Unacknowledged data (frames) also gives up after 10 s.
    set(libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, 10_000);
    // Keep little unsent data in the kernel (Linux would let it grow to megabytes):
    // frames wait in the outbox instead, where late ones can still be dropped.
    set(libc::IPPROTO_TCP, libc::TCP_NOTSENT_LOWAT, 256 * 1024);
}
