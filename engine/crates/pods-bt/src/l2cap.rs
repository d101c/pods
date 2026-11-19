// SPDX-License-Identifier: GPL-3.0-or-later
// Derived from podctl `src/l2cap.rs` (https://github.com/Rockykln/podctl),
// Copyright (c) 2026 pods contributors, MIT OR Apache-2.0. See engine/NOTICE.
// Changes: tokio `AsyncFd` instead of blocking reads, BT_SECURITY_MEDIUM and
// L2CAP_OPTIONS (imtu = omtu = 4096) set before connect, the `libc` crate.
//! The raw L2CAP socket of the AAP channel. No Bluetooth crate: the kernel
//! does the work, this file speaks the right sockaddr and options.
//!
//! **This is the only place in the engine that opens an `AF_BLUETOOTH`
//! socket.** Tests never come here: they use a fake [`crate::AapConnector`],
//! and [`forbid_real_bluetooth`] turns every attempt into an error.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

const BTPROTO_L2CAP: libc::c_int = 0;
const SOL_BLUETOOTH: libc::c_int = 274;
const BT_SECURITY: libc::c_int = 4;
const BT_SECURITY_MEDIUM: u8 = 2;
const SOL_L2CAP: libc::c_int = 6;
const L2CAP_OPTIONS: libc::c_int = 1;
const BDADDR_BREDR: u8 = 0x00;

/// Both directions: covers the 2 582 bytes RemoteHID sensor reports need.
pub const MTU: u16 = 4096;

/// Largest packet read in one go.
const RECV_BUF: usize = MTU as usize + 64;

static FORBIDDEN: AtomicBool = AtomicBool::new(false);

/// After this call, every [`L2capSocket::connect`] in the process fails
/// with `PermissionDenied`. Test harnesses call it first thing.
pub fn forbid_real_bluetooth() {
    FORBIDDEN.store(true, Ordering::SeqCst);
}

pub fn real_bluetooth_forbidden() -> bool {
    FORBIDDEN.load(Ordering::SeqCst)
}

/// `struct sockaddr_l2` of `<bluetooth/l2cap.h>`, padding included.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SockaddrL2 {
    l2_family: libc::sa_family_t,
    l2_psm: u16,
    l2_bdaddr: [u8; 6],
    l2_cid: u16,
    l2_bdaddr_type: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BtSecurity {
    level: u8,
    key_size: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct L2capOptions {
    omtu: u16,
    imtu: u16,
    flush_to: u16,
    mode: u8,
    fcs: u8,
    max_tx: u8,
    txwin_size: u16,
}

/// A connected AAP socket.
pub struct L2capSocket {
    fd: AsyncFd<OwnedFd>,
}

impl L2capSocket {
    /// Connects to `address` (`AA:BB:CC:DD:EE:FF`) on `psm`, giving up
    /// after `timeout`. The connect does not block the runtime.
    pub async fn connect(address: &str, psm: u16, timeout: Duration) -> io::Result<Self> {
        if real_bluetooth_forbidden() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "real Bluetooth sockets are forbidden in this process",
            ));
        }
        let bdaddr = parse_address(address)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad Bluetooth address"))?;

        // SAFETY: plain socket(2); the result is checked and owned at once.
        let raw = unsafe {
            libc::socket(
                libc::AF_BLUETOOTH,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                BTPROTO_L2CAP,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor nobody else holds.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        set_option(
            &fd,
            SOL_BLUETOOTH,
            BT_SECURITY,
            &BtSecurity {
                level: BT_SECURITY_MEDIUM,
                key_size: 0,
            },
        )?;
        let mut opts: L2capOptions = get_option(&fd, SOL_L2CAP, L2CAP_OPTIONS)?;
        opts.imtu = MTU;
        opts.omtu = MTU;
        set_option(&fd, SOL_L2CAP, L2CAP_OPTIONS, &opts)?;

        let addr = SockaddrL2 {
            l2_family: libc::AF_BLUETOOTH as libc::sa_family_t,
            l2_psm: psm.to_le(),
            l2_bdaddr: bdaddr,
            l2_cid: 0,
            l2_bdaddr_type: BDADDR_BREDR,
        };
        // SAFETY: `addr` is a valid sockaddr_l2 of the size given.
        let rc = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&raw const addr).cast::<libc::sockaddr>(),
                std::mem::size_of::<SockaddrL2>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(e);
            }
        }
        // SAFETY: the descriptor is owned by `fd` and moves into the
        // AsyncFd, so it cannot be closed while registered.
        let fd =
            unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE | Interest::WRITABLE) }
                .map_err(|e| io::Error::other(e.to_string()))?;
        tokio::time::timeout(timeout, wait_connected(&fd))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "L2CAP connect timed out"))??;
        Ok(Self { fd })
    }

    /// Sends one packet.
    pub async fn send(&self, packet: &[u8]) -> io::Result<()> {
        loop {
            let mut guard = self.fd.writable().await?;
            // SAFETY: writes from a valid slice to our own descriptor.
            match guard.try_io(|fd| {
                let n = unsafe {
                    libc::send(
                        fd.as_raw_fd(),
                        packet.as_ptr().cast(),
                        packet.len(),
                        libc::MSG_NOSIGNAL,
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(n)) if n == packet.len() => return Ok(()),
                Ok(Ok(n)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        format!("short L2CAP write: {n} of {}", packet.len()),
                    ));
                }
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }

    /// Receives one packet. End of stream is `UnexpectedEof`.
    /// Cancel-safe: nothing is read unless the call completes.
    pub async fn recv(&self) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; RECV_BUF];
        loop {
            let mut guard = self.fd.readable().await?;
            // SAFETY: reads into our own buffer from our own descriptor.
            match guard.try_io(|fd| {
                let n =
                    unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(0)) => {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "AAP closed"));
                }
                Ok(Ok(n)) => {
                    buf.truncate(n);
                    return Ok(buf);
                }
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }
}

async fn wait_connected(fd: &AsyncFd<OwnedFd>) -> io::Result<()> {
    loop {
        let mut guard = fd.writable().await?;
        let err: libc::c_int = get_option(fd.get_ref(), libc::SOL_SOCKET, libc::SO_ERROR)?;
        if err == libc::EINPROGRESS || err == libc::EALREADY {
            guard.clear_ready();
            continue;
        }
        if err != 0 {
            return Err(io::Error::from_raw_os_error(err));
        }
        return Ok(());
    }
}

fn set_option<T: Copy>(
    fd: &OwnedFd,
    level: libc::c_int,
    name: libc::c_int,
    value: &T,
) -> io::Result<()> {
    // SAFETY: `value` points to a `T` of the size given.
    let rc = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            level,
            name,
            (value as *const T).cast(),
            std::mem::size_of::<T>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn get_option<T: Copy + Default>(
    fd: &OwnedFd,
    level: libc::c_int,
    name: libc::c_int,
) -> io::Result<T> {
    let mut value = T::default();
    let mut len = std::mem::size_of::<T>() as libc::socklen_t;
    // SAFETY: `value` has room for the `len` bytes the kernel may write.
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            level,
            name,
            (&raw mut value).cast(),
            &raw mut len,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

/// `AA:BB:CC:DD:EE:FF` → bytes least significant first, as sockaddr_l2 wants.
pub fn parse_address(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        if part.len() != 2 {
            return None;
        }
        out[5 - i] = u8::from_str_radix(part, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_reversed() {
        assert_eq!(
            parse_address("AA:BB:CC:DD:EE:FF"),
            Some([0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA])
        );
        assert_eq!(
            parse_address("aa:bb:cc:dd:ee:0f"),
            Some([0x0F, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA])
        );
        assert_eq!(parse_address("AA:BB:CC:DD:EE"), None);
        assert_eq!(parse_address("AA:BB:CC:DD:EE:FFF"), None);
        assert_eq!(parse_address("not an address"), None);
    }

    #[test]
    fn struct_layouts_match_the_kernel() {
        assert_eq!(std::mem::size_of::<SockaddrL2>(), 14);
        assert_eq!(std::mem::size_of::<L2capOptions>(), 12);
        assert_eq!(std::mem::size_of::<BtSecurity>(), 2);
    }

    /// The guard stops a connect before any socket exists.
    #[tokio::test]
    async fn the_guard_refuses_real_sockets() {
        forbid_real_bluetooth();
        let err = L2capSocket::connect("00:00:00:00:00:00", 0x1001, Duration::from_millis(10))
            .await
            .err()
            .expect("refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }
}
