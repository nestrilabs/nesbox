//! A tap interface, the host end of the guest's network link.
//!
//! Taps are *opened*, never created. A tap the host set up beforehand and gave
//! to the user nesbox runs as can be attached to with no privilege at all --
//! the kernel demands `CAP_NET_ADMIN` only to create a device, or when the
//! opener is not its owner (`tun_not_capable` in `drivers/net/tun.c`). Creating
//! them here would mean shipping a VMM that wants net-admin on its binary,
//! which is a thing to ask of everyone who self-hosts.
//!
//! `scripts/nestri-net-setup.sh` creates them. The interface is not persistent: it
//! exists only while we hold the file descriptor, so it disappears when the VM
//! exits however the VM exits, including a crash. Nothing has to clean up
//! after us.
//!
//! What is *not* here, and cannot be: routing the tap's subnet to the outside
//! world. That is `ip_forward` plus a NAT rule, which is host-global state and
//! belongs to whoever installs nesbox, not to a running VM.

use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};

const TUN_PATH: &str = "/dev/net/tun";

// ioctls, from linux/if_tun.h. libc does not export these for gnu targets.
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
const TUNSETOFFLOAD: libc::c_ulong = 0x4004_54d0;
const TUNSETVNETHDRSZ: libc::c_ulong = 0x4004_54d8;

/// Add an interface to a bridge. `ifr_name` is the *bridge*, and the payload
/// carries the ifindex of the interface being added.

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
    /// Write the interface flags, the one payload member still used: TUNSETIFF
    /// takes the tap's flags there.
    fn set_flags(&mut self, flags: i16) {
        let bytes = flags.to_ne_bytes();
        self.payload[0] = (self.payload[0] & !0xffff) | u64::from(u16::from_ne_bytes(bytes));
    }

    fn new(name: &str) -> Result<Self> {
        // The kernel needs room for a terminating NUL.
        if name.len() >= IFNAMSIZ {
            bail!("interface name {name:?} is too long, max {} bytes", IFNAMSIZ - 1);
        }
        let mut req = Self { name: [0; IFNAMSIZ], payload: [0; 3] };
        req.name[..name.len()].copy_from_slice(name.as_bytes());
        Ok(req)
    }





}

/// A tap interface. Dropping this destroys the interface.
pub struct Tap {
    file: File,
    name: String,
}

impl Tap {
    /// Attach to a tap the host already created.
    ///
    /// `name` must be exact. `%d` is gone with creation: letting the kernel
    /// pick a name is only possible when creating the device, and creating is
    /// what needed the privilege.
    /// # A persistent tap can have more than one opener
    ///
    /// `TUNSETIFF` on a name that already exists attaches to it; it does not
    /// fail because somebody else is attached. So a VM relaunched before the
    /// previous process has exited can end up as the *second* opener of the
    /// same tap, and the kernel steers frames between the queues rather than to
    /// the one that wants them.
    ///
    /// **Observed once and not reproduced since**: a guest relaunched
    /// immediately after being stopped came up with working configuration and
    /// no traffic, and the same launch worked when it was not rushed. The
    /// explanation above fits and is unconfirmed — nothing here has been
    /// changed on the strength of it.
    ///
    /// If it recurs, the check that settles it is `lsof /dev/net/tun` between
    /// the stop and the start: a nesbox process still listed there is the
    /// stale opener. The fix would be to close the tap explicitly on teardown
    /// rather than leaving it to process exit, and for the caller to wait for
    /// the previous process to be reaped before reusing its slot. Until then,
    /// leaving 5-10 seconds between stopping a VM and starting it again avoids
    /// the window entirely.
    pub fn open(name: &str) -> Result<Self> {
        if !std::path::Path::new(&format!("/sys/class/net/{name}")).exists() {
            bail!(
                "tap {name} does not exist. nesbox does not create taps -- run \
                 scripts/nestri-net-setup.sh on this host, which makes them and \
                 hands them to the user nesbox runs as"
            );
        }

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
                    "TUNSETIFF on {name} denied. The tap exists but is owned by \
                     someone else -- a tap may be opened without privilege only by \
                     its owner. Re-run scripts/nestri-net-setup.sh naming the user \
                     nesbox runs as."
                );
            }
            return Err(err).context("TUNSETIFF");
        }

        Ok(Self {
            file,
            name: name.to_string(),
        })
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
}

impl AsRawFd for Tap {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

