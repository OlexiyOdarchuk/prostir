//! USB devices plugged in, as the kernel tells (its uevents, over netlink): a thread
//! sleeps on the socket until a device comes, instead of asking adb every few seconds.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use log::warn;

/// The kernel's own uevents (udev passes them on in group 2, which needs udev).
const KERNEL_EVENTS: u32 = 1;

/// Whether a uevent says a USB device was plugged in or became ready (a driver bound,
/// its configuration changed). Its interfaces' events say nothing more.
fn usb_device_arrived(event: &[u8]) -> bool {
    let (mut action, mut subsystem, mut devtype) = (None, None, None);
    // The header (`add@/devices/…`) first, then KEY=value fields.
    for field in event.split(|b| *b == 0).skip(1) {
        if let Some(v) = field.strip_prefix(b"ACTION=") {
            action = Some(v);
        } else if let Some(v) = field.strip_prefix(b"SUBSYSTEM=") {
            subsystem = Some(v);
        } else if let Some(v) = field.strip_prefix(b"DEVTYPE=") {
            devtype = Some(v);
        }
    }
    subsystem == Some(b"usb")
        && devtype == Some(b"usb_device")
        && matches!(action, Some(b"add" | b"bind" | b"change"))
}

/// Calls `on_arrival` on a thread of its own each time a USB device is plugged in or
/// becomes ready. An error when the kernel's events cannot be read.
pub fn on_usb_arrival(on_arrival: impl Fn() + Send + 'static) -> std::io::Result<()> {
    // SAFETY: a plain socket call; the descriptor is owned right below.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a new descriptor nothing else owns.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: an all-zero sockaddr_nl is valid; its family and group are set below.
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_groups = KERNEL_EVENTS;
    // SAFETY: binds the socket to an address of the size given.
    let bound = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&raw const addr).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if bound != 0 {
        return Err(std::io::Error::last_os_error());
    }
    std::thread::Builder::new()
        .name("usb".into())
        .spawn(move || {
            let mut buf = vec![0u8; 8192];
            loop {
                // SAFETY: receives into a buffer of the length given.
                let n = unsafe {
                    libc::recv(socket.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0)
                };
                if n < 0 {
                    let e = std::io::Error::last_os_error();
                    match e.raw_os_error() {
                        // Interrupted, or events lost while the queue was full: read on.
                        Some(libc::EINTR | libc::ENOBUFS) => continue,
                        _ => {
                            warn!("USB devices are no longer followed: {e}");
                            return;
                        }
                    }
                }
                if usb_device_arrived(&buf[..n as usize]) {
                    on_arrival();
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(fields: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for f in fields {
            out.extend_from_slice(f.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn a_usb_device_plugged_in_is_noticed() {
        let plugged = event(&[
            "add@/devices/pci0000:00/0000:00:14.0/usb1/1-2",
            "ACTION=add",
            "DEVPATH=/devices/pci0000:00/0000:00:14.0/usb1/1-2",
            "SUBSYSTEM=usb",
            "DEVNAME=bus/usb/001/005",
            "DEVTYPE=usb_device",
            "PRODUCT=2833/186/ffff",
            "SEQNUM=4510",
        ]);
        assert!(usb_device_arrived(&plugged));
        let bound = event(&[
            "bind@/x",
            "ACTION=bind",
            "SUBSYSTEM=usb",
            "DEVTYPE=usb_device",
        ]);
        assert!(usb_device_arrived(&bound));
        // Its interfaces, removal, other subsystems: nothing to do.
        let interface = event(&[
            "add@/x/1-2:1.0",
            "ACTION=add",
            "SUBSYSTEM=usb",
            "DEVTYPE=usb_interface",
        ]);
        assert!(!usb_device_arrived(&interface));
        let removed = event(&[
            "remove@/x",
            "ACTION=remove",
            "SUBSYSTEM=usb",
            "DEVTYPE=usb_device",
        ]);
        assert!(!usb_device_arrived(&removed));
        let block = event(&["add@/x", "ACTION=add", "SUBSYSTEM=block", "DEVTYPE=disk"]);
        assert!(!usb_device_arrived(&block));
        // The header alone does not count, nor does garbage.
        assert!(!usb_device_arrived(b"add@/devices/usb1/1-2"));
        assert!(!usb_device_arrived(&[0xff, 0, 0xfe]));
    }
}
