//! A tap interface, the host end of the guest's network link.
//!
//! Creating a tap needs `CAP_NET_ADMIN`. The interface is not persistent: it
//! exists only while we hold the file descriptor, so it disappears when the VM
//! exits however the VM exits, including a crash. Nothing has to clean up
//! after us.
//!
//! What is *not* here, and cannot be: routing the tap's subnet to the outside
//! world. That is `ip_forward` plus a NAT rule, which is host-global state and
//! belongs to whoever installs nesbox, not to a running VM.

use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::net::Ipv4Addr;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};

const TUN_PATH: &str = "/dev/net/tun";

// ioctls, from linux/if_tun.h. libc does not export these for gnu targets.
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
const TUNSETOFFLOAD: libc::c_ulong = 0x4004_54d0;
const TUNSETVNETHDRSZ: libc::c_ulong = 0x4004_54d8;

// Socket ioctls, from linux/sockios.h.
const SIOCSIFADDR: libc::c_ulong = 0x8916;
const SIOCSIFNETMASK: libc::c_ulong = 0x891c;
const SIOCSIFFLAGS: libc::c_ulong = 0x8914;
const SIOCGIFFLAGS: libc::c_ulong = 0x8913;

/// Offloads the tap may pass through without doing the work itself. Which of
/// these we ask for follows from the features the guest accepted: the guest is
/// promising it can handle these frames, so the kernel need not fix them up.
pub const TUN_F_CSUM: u32 = 0x01;
pub const TUN_F_TSO4: u32 = 0x02;
pub const TUN_F_TSO6: u32 = 0x04;
pub const TUN_F_TSO_ECN: u32 = 0x08;
pub const TUN_F_UFO: u32 = 0x10;

const IFNAMSIZ: usize = 16;

/// `struct ifreq`. The payload is a union in C; we only ever fill one member,
/// so it is laid out here as three words to get the size and alignment right.
#[repr(C)]
#[derive(Clone, Copy)]
struct IfReq {
    name: [u8; IFNAMSIZ],
    payload: [u64; 3],
}

impl IfReq {
    fn new(name: &str) -> Result<Self> {
        // The kernel needs room for a terminating NUL.
        if name.len() >= IFNAMSIZ {
            bail!("interface name {name:?} is too long, max {} bytes", IFNAMSIZ - 1);
        }
        let mut req = Self { name: [0; IFNAMSIZ], payload: [0; 3] };
        req.name[..name.len()].copy_from_slice(name.as_bytes());
        Ok(req)
    }

    /// Write a `sockaddr_in` into the payload, for the address ioctls.
    fn set_addr(&mut self, addr: Ipv4Addr) {
        let sa = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: 0,
            sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(addr.octets()) },
            sin_zero: [0; 8],
        };
        // SAFETY: sockaddr_in is 16 bytes, the payload is 24 and aligned to 8.
        unsafe {
            std::ptr::copy_nonoverlapping(
                &sa as *const libc::sockaddr_in as *const u8,
                self.payload.as_mut_ptr() as *mut u8,
                std::mem::size_of::<libc::sockaddr_in>(),
            );
        }
    }

    fn flags(&self) -> i16 {
        i16::from_ne_bytes([self.payload[0] as u8, (self.payload[0] >> 8) as u8])
    }

    fn set_flags(&mut self, flags: i16) {
        let bytes = flags.to_ne_bytes();
        self.payload[0] = (self.payload[0] & !0xffff) | u64::from(u16::from_ne_bytes(bytes));
    }
}

/// A tap interface. Dropping this destroys the interface.
pub struct Tap {
    file: File,
    name: String,
}

impl Tap {
    /// Create a tap interface. `name` may end in `%d`, which the kernel fills
    /// in with the lowest free number.
    pub fn create(name: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(TUN_PATH)
            .with_context(|| format!("failed to open {TUN_PATH}"))?;

        let mut req = IfReq::new(name)?;
        // IFF_NO_PI: no protocol header before each frame, virtio does not
        // want one. IFF_VNET_HDR: frames carry a virtio_net_hdr, which is what
        // makes offloads possible at all.
        req.set_flags((libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_VNET_HDR) as i16);

        // SAFETY: the fd is open and req outlives the call.
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), TUNSETIFF, &mut req) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EPERM) {
                bail!(
                    "TUNSETIFF denied — creating a tap needs CAP_NET_ADMIN. \
                     Grant it with: sudo setcap cap_net_admin+ep <path to nesbox>"
                );
            }
            return Err(err).context("TUNSETIFF");
        }

        // The kernel writes back the name it actually assigned.
        let end = req.name.iter().position(|&b| b == 0).unwrap_or(IFNAMSIZ);
        let name = String::from_utf8_lossy(&req.name[..end]).into_owned();
        log::info!("created tap {name}");
        Ok(Self { file, name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    /// Tell the tap how many bytes of `virtio_net_hdr` precede each frame.
    /// This must match what the guest expects or every frame is misparsed.
    pub fn set_vnet_hdr_size(&self, size: i32) -> Result<()> {
        // SAFETY: the fd is open and size outlives the call.
        let ret = unsafe { libc::ioctl(self.file.as_raw_fd(), TUNSETVNETHDRSZ, &size) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error()).context("TUNSETVNETHDRSZ");
        }
        Ok(())
    }

    /// Enable the offloads the guest said it can cope with.
    pub fn set_offload(&self, flags: u32) -> Result<()> {
        // SAFETY: the fd is open; TUNSETOFFLOAD takes the value directly.
        let ret = unsafe { libc::ioctl(self.file.as_raw_fd(), TUNSETOFFLOAD, flags as libc::c_ulong) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error()).context("TUNSETOFFLOAD");
        }
        Ok(())
    }

    /// Give the host end an address and bring the link up.
    pub fn configure(&self, ip: Ipv4Addr, netmask: Ipv4Addr) -> Result<()> {
        let sock = UdpSocketFd::new()?;

        let mut req = IfReq::new(&self.name)?;
        req.set_addr(ip);
        sock.ioctl(SIOCSIFADDR, &mut req).context("SIOCSIFADDR")?;

        let mut req = IfReq::new(&self.name)?;
        req.set_addr(netmask);
        sock.ioctl(SIOCSIFNETMASK, &mut req).context("SIOCSIFNETMASK")?;

        // Read the current flags before setting UP, so we do not clear the
        // ones the kernel already set.
        let mut req = IfReq::new(&self.name)?;
        sock.ioctl(SIOCGIFFLAGS, &mut req).context("SIOCGIFFLAGS")?;
        let flags = req.flags() | (libc::IFF_UP | libc::IFF_RUNNING) as i16;
        req.set_flags(flags);
        sock.ioctl(SIOCSIFFLAGS, &mut req).context("SIOCSIFFLAGS")?;

        log::info!("tap {} is up at {ip}/{netmask}", self.name);
        Ok(())
    }
}

impl AsRawFd for Tap {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

/// A socket held open only so interface ioctls have something to act on.
struct UdpSocketFd(RawFd);

impl UdpSocketFd {
    fn new() -> Result<Self> {
        // SAFETY: plain socket creation, return value checked.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("socket(AF_INET)");
        }
        Ok(Self(fd))
    }

    fn ioctl(&self, request: libc::c_ulong, req: &mut IfReq) -> Result<()> {
        // SAFETY: the fd is open and req outlives the call.
        let ret = unsafe { libc::ioctl(self.0, request, req) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

impl Drop for UdpSocketFd {
    fn drop(&mut self) {
        // SAFETY: we own this fd and it is not used again.
        unsafe { libc::close(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_names_must_fit() {
        assert!(IfReq::new("tap0").is_ok());
        assert!(IfReq::new("nesbox%d").is_ok());
        // 15 characters is the longest that still leaves room for the NUL.
        assert!(IfReq::new("abcdefghijklmno").is_ok());
        assert!(IfReq::new("abcdefghijklmnop").is_err());
    }

    #[test]
    fn name_is_nul_terminated_in_the_request() {
        let req = IfReq::new("tap0").unwrap();
        assert_eq!(&req.name[..4], b"tap0");
        assert!(req.name[4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn flags_round_trip() {
        let mut req = IfReq::new("tap0").unwrap();
        req.set_flags((libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_VNET_HDR) as i16);
        assert_eq!(req.flags(), (libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_VNET_HDR) as i16);
    }

    #[test]
    fn ifreq_matches_the_kernel_layout() {
        assert_eq!(std::mem::size_of::<IfReq>(), 40);
        assert_eq!(std::mem::align_of::<IfReq>(), 8);
    }
}
