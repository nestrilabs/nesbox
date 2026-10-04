//! GPU ioctl forwarding device over PCI transport, backed by a vhost-user daemon.
//!
//! The guest runs the GPU vendor's own user-mode driver and issues its ordinary
//! device ioctls. This device carries them to a backend process that holds the
//! real host device descriptors and replays them there. Like virtio-fs, the
//! transport is emulated here and the work happens in a separate process, so
//! guest RAM has to be shared memory.
//!
//! Two things make this device's shape differ from the others here.
//!
//! **Device config comes from the backend, once, before the guest boots.** The
//! guest driver reads a structure describing the host driver version, the GPUs,
//! which capabilities are served, and which ioctls carry a file descriptor. Only
//! the backend knows the last two: it decides what it serves, and the
//! descriptor table grows with what it can translate. So it is fetched over
//! `VHOST_USER_GET_CONFIG` when the device is created, because the guest reads
//! config during probe, before it sets DRIVER_OK.
//!
//! Two independent 4 KiB limits meet here, which is worth knowing before
//! anyone tries to grow this structure: the vhost-user config message caps
//! `offset + size` at 4096, and a guest maps device config with PAGE_SIZE as
//! its maximum. Neither is in this code, and neither can be raised from it.
//!
//! **It cannot use the common BAR layout.** That layout leaves 256 bytes
//! between the ISR and notify regions for device config, which is 35 times too
//! small here. The offsets below are therefore local to this device. They are
//! ordered with the small regions first and config last, so config can grow
//! without moving anything else.

use crate::common::*;
use crate::memmap::HostMemoryMapper;
use anyhow::{Context, Result};
use pci::config::{PCIE_TYPE_RC_INTEGRATED, PciConfig};
use pci::{BarType, Doorbell, MsiRouter, MsiVector, PciDevice};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::{Arc, Mutex};
use vhost::vhost_user::message::{
    VhostUserConfigFlags, VhostUserMMap, VhostUserMMapFlags, VhostUserProtocolFeatures,
    VhostUserVirtioFeatures,
};
use vhost::vhost_user::{
    Frontend, FrontendReqHandler, HandlerResult, VhostUserFrontend, VhostUserFrontendReqHandler,
};
use vhost::{VhostBackend, VhostUserMemoryRegionInfo, VringConfigData};
use vm_memory::{Address, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};
use vmm_sys_util::eventfd::EventFd;

const QUEUE_SIZE: u16 = 256;

/// A control queue for guest requests and an event queue for device-initiated
/// notifications. The guest driver asks for exactly two and fails to probe on
/// the error if it gets fewer.
const NUM_QUEUES: usize = 2;

/// One per queue plus a config vector.
const MSIX_VECTORS: u16 = 3;

/// virtio device type 45; PCI device id is 0x1040 + type.
const VIRTIO_ID_GPU_NV: u16 = 45;
const PCI_DEVICE_ID: u16 = 0x1040 + VIRTIO_ID_GPU_NV;

/// Size of the guest-visible device configuration structure.
///
/// It must fit in one page. A guest maps the device config capability with
/// PAGE_SIZE as its maximum and silently truncates anything longer, so a
/// larger layout is not merely wasteful: every field past 4096 reads back out
/// of range and takes the guest driver down inside virtio_cread_bytes.
const CONFIG_LEN: usize = 4024;
/// What a backend from before `vram_limit_mib` serves. Its config is the same
/// up to here, and the missing field reads as 0, "no limit".
const CONFIG_LEN_NO_VRAM_LIMIT: usize = 4016;

// Fields of the device config this side checks before a guest sees it.
const OFF_NUM_GPUS: usize = 32;
const MAX_GPUS: u32 = 8;
const OFF_NUM_FD_TRANSLATIONS: usize = 3880;
const MAX_FD_TRANSLATIONS: u32 = 16;
const OFF_VRAM_LIMIT_MIB: usize = 4016;

// ── BAR 0 layout, local to this device ──────────────────────────────────────
// Small regions first, config last and page aligned, so growing config moves
// nothing else.
const NV_OFF_COMMON: u64 = 0x0000;
const NV_OFF_ISR: u64 = 0x0100;
const NV_OFF_NOTIFY: u64 = 0x0200;
const NV_OFF_MSIX_TABLE: u64 = 0x0300;
const NV_OFF_MSIX_PBA: u64 = 0x0400;
const NV_OFF_DEVICE: u64 = 0x1000;
const NV_BAR0_SIZE: u64 = 0x4000;

const _: () = assert!(
    NV_OFF_DEVICE + CONFIG_LEN as u64 <= NV_BAR0_SIZE,
    "device config does not fit in BAR 0"
);

/// BAR 2 is the shared window device memory appears in. BAR 1 is skipped
/// because BAR 0 is 32-bit and BAR 2's high half occupies BAR 3.
const SHM_BAR: usize = 2;

/// The shared-memory id the guest driver looks the window up by. Zero is the
/// "undefined" id, which a guest discards without a word.
const NV_SHM_ID: u8 = 1;

/// Size of the window, which must cover every offset the backend's allocator
/// can hand out -- its three zones total exactly this.
///
/// Nothing is committed for it here. The reservation is PROT_NONE and the
/// pages arrive only as the backend asks for them, one mapping at a time.
const SHM_SIZE: u64 = 1 << 30; // 1 GiB

/// BAR 4 is the UVM aperture: one memory slot per CUDA semaphore pool.
///
/// A pool cannot go in the window. CUDA makes it at an address of its own
/// choosing and maps the UVM file there at an offset equal to that address,
/// and UVM takes the mapping at that host address and nowhere else. The window
/// is one reservation at an address this process picked, so each pool is
/// mapped at its own address instead and given a slot inside this BAR, at an
/// offset the backend chose. Nothing backs the BAR until then.
const APERTURE_BAR: usize = 4;
const NV_SHM_ID_APERTURE: u8 = 2;
const APERTURE_SIZE: u64 = 1 << 30;
/// Pools start on 2 MiB boundaries of the aperture.
const APERTURE_ALIGN: u64 = 2 << 20;
/// One pool, and all of them together. CUDA makes them at a few MiB.
const POOL_MAX_LEN: u64 = 64 << 20;
const MAX_POOLS: usize = 64;
const MAX_POOL_BYTES: u64 = 256 << 20;
/// The host addresses a pool may name. Below 4 GiB is where this process's own
/// small mappings sit; the mapping is made without replacing anything anyway,
/// so the band is a second wall, not the only one.
const POOL_HVA_MIN: u64 = 4 << 30;
const POOL_HVA_MAX: u64 = 32 << 40;

/// Places backend mappings into the shared window.
///
/// The backend holds the real device descriptors, but it cannot do this
/// placement itself. `MAP_FIXED` rewrites the calling process's page tables and
/// nothing else, so a mapping made over there would never appear in the memory
/// slot registered from here -- the guest would read the window's own zero
/// pages and see no device at all. The descriptor therefore travels up and the
/// mapping is made in this address space, which is the one the slot describes.
struct WindowMapper {
    mapper: Arc<dyn HostMemoryMapper>,
    /// Guest physical base of BAR 2, known only once the bus assigns it.
    guest_base: Mutex<Option<u64>>,
    /// Guest physical base of BAR 4, likewise.
    aperture_base: Mutex<Option<u64>>,
    /// Pools placed in the aperture: offset -> (length, host address).
    pools: Mutex<std::collections::BTreeMap<u64, (u64, u64)>>,
}

/// Whether a pool may be placed, given the ones already there.
fn check_pool(
    pools: &std::collections::BTreeMap<u64, (u64, u64)>,
    hva: u64,
    offset: u64,
    len: u64,
    flags: u64,
) -> std::result::Result<(), i32> {
    let page = 4096;
    if flags != VhostUserMMapFlags::WRITABLE.bits()
        || len == 0
        || len % page != 0
        || len > POOL_MAX_LEN
        || hva % page != 0
        || hva < POOL_HVA_MIN
        || hva.checked_add(len).is_none_or(|end| end > POOL_HVA_MAX)
        || offset % APERTURE_ALIGN != 0
        || offset
            .checked_add(len)
            .is_none_or(|end| end > APERTURE_SIZE)
    {
        return Err(libc::EINVAL);
    }
    let hit = |s: u64, l: u64, a: u64| s < a + len && a < s + l;
    if pools
        .iter()
        .any(|(&o, &(l, h))| hit(o, l, offset) || hit(h, l, hva))
    {
        return Err(libc::EEXIST);
    }
    let bytes: u64 = pools.values().map(|&(l, _)| l).sum();
    if pools.len() >= MAX_POOLS || bytes + len > MAX_POOL_BYTES {
        return Err(libc::ENOSPC);
    }
    Ok(())
}

impl WindowMapper {
    fn place(&self, req: &VhostUserMMap, fd: RawFd) -> std::io::Result<()> {
        // Copied out first: the message is `repr(packed)`, so a reference to a
        // field of it is unaligned and cannot be formatted or borrowed.
        let (shm_offset, len, fd_offset, flags) =
            (req.shm_offset, req.len, req.fd_offset, req.flags);

        let base = self
            .guest_base
            .lock()
            .unwrap()
            .ok_or_else(|| std::io::Error::other("BAR 2 has no address yet"))?;

        let guest_addr = base
            .checked_add(shm_offset)
            .ok_or_else(|| std::io::Error::other("window offset overflows"))?;

        let host = self.mapper.host_addr(guest_addr, len).ok_or_else(|| {
            std::io::Error::other(format!("{shm_offset:#x}+{len:#x} is outside the window"))
        })?;

        let prot = if flags & VhostUserMMapFlags::WRITABLE.bits() != 0 {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };

        // SAFETY: `host` is inside the reservation this mapper owns, and the
        // length was checked against it above.
        let p = unsafe {
            libc::mmap(
                host as *mut libc::c_void,
                len as usize,
                prot,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                fd_offset as i64,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        log::debug!("window: placed {shm_offset:#x}+{len:#x} (guest {guest_addr:#x})");
        Ok(())
    }
}

impl WindowMapper {
    /// Map a UVM file at the pool's own address and give it a slot in the
    /// aperture.
    fn place_pool(&self, req: &VhostUserMMap, fd: RawFd) -> std::io::Result<()> {
        let (hva, offset, len, flags) = (req.fd_offset, req.shm_offset, req.len, req.flags);
        let base = self
            .aperture_base
            .lock()
            .unwrap()
            .ok_or_else(|| std::io::Error::other("BAR 4 has no address yet"))?;
        let mut pools = self.pools.lock().unwrap();
        if let Err(errno) = check_pool(&pools, hva, offset, len, flags) {
            log::error!(
                "refusing UVM pool {hva:#x}+{len:#x} at aperture offset {offset:#x}: errno {errno}"
            );
            return Err(std::io::Error::from_raw_os_error(errno));
        }

        // Never over anything of ours: MAP_FIXED_NOREPLACE fails rather than
        // replace a mapping already at that address.
        //
        // SAFETY: the kernel refuses rather than replaces, so no existing
        // mapping of this process is touched; the descriptor is borrowed for
        // the call only.
        let p = unsafe {
            libc::mmap(
                hva as *mut libc::c_void,
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED_NOREPLACE,
                fd,
                hva as i64,
            )
        };
        if p == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            log::error!("UVM pool {hva:#x}+{len:#x}: mmap: {e}");
            return Err(e);
        }
        let unmap = || {
            // SAFETY: the mapping made just above, same address and length.
            unsafe { libc::munmap(p, len as usize) };
        };
        // A kernel older than MAP_FIXED_NOREPLACE treats it as a hint.
        if p as u64 != hva {
            unmap();
            return Err(std::io::Error::from_raw_os_error(libc::EEXIST));
        }
        // The pages must be there before a slot points at them: UVM inserts
        // them when it maps a pool, and a range it cannot fault is refused
        // here rather than met by the guest.
        //
        // SAFETY: advice on the range mapped above.
        if unsafe { libc::madvise(p, len as usize, libc::MADV_POPULATE_WRITE) } != 0 {
            let e = std::io::Error::last_os_error();
            unmap();
            log::error!("UVM pool {hva:#x}+{len:#x}: the pages are not there: {e}");
            return Err(e);
        }
        if let Err(e) = self.mapper.map(base + offset, hva, len) {
            unmap();
            return Err(std::io::Error::other(format!("{e:#}")));
        }
        pools.insert(offset, (len, hva));
        log::debug!("aperture: pool {hva:#x}+{len:#x} at offset {offset:#x}");
        Ok(())
    }

    /// The slot first, then the mapping, so the guest never has a slot over
    /// nothing.
    fn withdraw_pool(&self, offset: u64, len: u64) -> std::io::Result<()> {
        let base = self
            .aperture_base
            .lock()
            .unwrap()
            .ok_or_else(|| std::io::Error::other("BAR 4 has no address yet"))?;
        let mut pools = self.pools.lock().unwrap();
        let Some(&(l, hva)) = pools.get(&offset) else {
            return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
        };
        if l != len {
            return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
        }
        self.mapper
            .unmap(base + offset, len)
            .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
        // SAFETY: the pool's own mapping, made by `place_pool`.
        unsafe { libc::munmap(hva as *mut libc::c_void, len as usize) };
        pools.remove(&offset);
        Ok(())
    }

    /// Every pool, when the backend goes away.
    fn withdraw_all_pools(&self) {
        let offsets: Vec<(u64, u64)> = self
            .pools
            .lock()
            .unwrap()
            .iter()
            .map(|(&o, &(l, _))| (o, l))
            .collect();
        for (offset, len) in offsets {
            if let Err(e) = self.withdraw_pool(offset, len) {
                log::warn!("aperture: pool at {offset:#x} not withdrawn: {e}");
            }
        }
    }
}

impl VhostUserFrontendReqHandler for WindowMapper {
    fn shmem_map(&self, req: &VhostUserMMap, fd: &dyn AsRawFd) -> HandlerResult<u64> {
        match req.shmid {
            NV_SHM_ID => self.place(req, fd.as_raw_fd()).map(|()| 0),
            NV_SHM_ID_APERTURE => self.place_pool(req, fd.as_raw_fd()).map(|()| 0),
            _ => Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
        }
    }

    fn shmem_unmap(&self, req: &VhostUserMMap) -> HandlerResult<u64> {
        match req.shmid {
            NV_SHM_ID => {}
            NV_SHM_ID_APERTURE => {
                let (offset, len) = (req.shm_offset, req.len);
                return self.withdraw_pool(offset, len).map(|()| 0);
            }
            _ => return Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
        }
        // Overwrite rather than unmap: leaving a hole would let a later fault
        // in this range reach no VMA at all, and the slot still describes it.
        let base = self
            .guest_base
            .lock()
            .unwrap()
            .ok_or_else(|| std::io::Error::other("BAR 2 has no address yet"))?;
        let (shm_offset, len) = (req.shm_offset, req.len);
        let guest_addr = base.saturating_add(shm_offset);
        self.mapper
            .withdraw(guest_addr, len)
            .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
        Ok(0)
    }
}

struct Inner {
    com: ComCfg,
    qs: u16,
    queues: [QState; NUM_QUEUES],
    isr: u8,
    cfg_vec: u16,
    mem: Arc<GuestMemoryMmap>,
    msix: MsixTable<MSIX_VECTORS_USIZE>,
    cfg: [u8; 256],
    msix_cap: u16,
    frontend: Frontend,
    /// Present once a mapper is bound. Without it the backend is never given a
    /// request channel and keeps its mappings to itself.
    window: Option<Arc<WindowMapper>>,
    kick_fds: Vec<Arc<EventFd>>,
    running: bool,
    /// Device config as the backend reported it at creation.
    ///
    /// Cached rather than re-read per access: the guest reads config in small
    /// pieces during probe, and a socket round trip for each would turn one
    /// description of the host into hundreds.
    device_config: Vec<u8>,
}

const MSIX_VECTORS_USIZE: usize = MSIX_VECTORS as usize;

impl Inner {
    fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1
    }

    /// Complete the vhost-user handshake and hand over the queues.
    fn activate(&mut self) -> Result<()> {
        if self.running {
            return Ok(());
        }

        // SET_OWNER was sent when the device was created, to fetch config, and
        // a backend refuses a second one. Only features are negotiated here.
        let backend_features = self
            .frontend
            .get_features()
            .context("VHOST_USER_GET_FEATURES")?;
        let protocol_bit = VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();
        let acked = (self.com.df & self.features()) | (backend_features & protocol_bit);
        self.frontend
            .set_features(acked)
            .context("VHOST_USER_SET_FEATURES")?;

        if backend_features & protocol_bit != 0 {
            let offered = self
                .frontend
                .get_protocol_features()
                .context("VHOST_USER_GET_PROTOCOL_FEATURES")?;
            // Only the ones we implement. BACKEND_REQ opens the channel the
            // backend asks for mappings on, and SHMEM is what gates the
            // mapping request itself -- the backend refuses to send one
            // without it, so asking for the channel alone achieves nothing.
            let mut wanted = VhostUserProtocolFeatures::MQ
                | VhostUserProtocolFeatures::REPLY_ACK
                | VhostUserProtocolFeatures::CONFIG;
            // The backend only gets a request channel if there is a window for
            // it to place mappings in. Without one it must keep every mapping
            // to itself, which is the state this device shipped in.
            if self.window.is_some() {
                wanted |= VhostUserProtocolFeatures::BACKEND_REQ | VhostUserProtocolFeatures::SHMEM;
            }
            let agreed = offered & wanted;
            self.frontend
                .set_protocol_features(agreed)
                .context("VHOST_USER_SET_PROTOCOL_FEATURES")?;

            if agreed.contains(VhostUserProtocolFeatures::BACKEND_REQ) {
                self.start_backend_requests()?;
            }
        }

        let regions = self.memory_regions()?;
        self.frontend
            .set_mem_table(&regions)
            .context("VHOST_USER_SET_MEM_TABLE")?;

        for (idx, queue) in self.queues.iter().enumerate() {
            anyhow::ensure!(
                queue.desc != 0 && queue.avail != 0 && queue.used != 0,
                "queue {idx} was never programmed by the driver"
            );
            // vhost-user vring addresses are in *our* address space, not the
            // guest's: the memory table tells the backend how to map them back.
            let desc = self.host_addr(queue.desc).context("queue desc address")?;
            let avail = self.host_addr(queue.avail).context("queue avail address")?;
            let used = self.host_addr(queue.used).context("queue used address")?;

            self.frontend
                .set_vring_num(idx, queue.size)
                .context("VHOST_USER_SET_VRING_NUM")?;
            self.frontend
                .set_vring_addr(
                    idx,
                    &VringConfigData {
                        queue_max_size: QUEUE_SIZE,
                        queue_size: queue.size,
                        flags: 0,
                        desc_table_addr: desc,
                        used_ring_addr: used,
                        avail_ring_addr: avail,
                        log_addr: None,
                    },
                )
                .context("VHOST_USER_SET_VRING_ADDR")?;
            self.frontend
                .set_vring_base(idx, queue.last)
                .context("VHOST_USER_SET_VRING_BASE")?;
            self.frontend
                .set_vring_kick(idx, &self.kick_fds[idx])
                .context("VHOST_USER_SET_VRING_KICK")?;
            let call_fd = self
                .msix
                .call_fd(queue.vec)
                .context("queue has no interrupt to signal")?;
            self.frontend
                .set_vring_call(idx, call_fd)
                .context("VHOST_USER_SET_VRING_CALL")?;
        }

        // Only now may the backend touch the rings.
        for idx in 0..NUM_QUEUES {
            self.frontend
                .set_vring_enable(idx, true)
                .context("VHOST_USER_SET_VRING_ENABLE")?;
        }

        self.running = true;
        log::info!("virtio-gpu-nv active");
        Ok(())
    }

    /// Translate a guest physical address into this process's address space.
    fn host_addr(&self, gpa: u64) -> Result<u64> {
        let host = self
            .mem
            .get_host_address(vm_memory::GuestAddress(gpa))
            .with_context(|| format!("no host mapping for guest address {gpa:#x}"))?;
        Ok(host as u64)
    }

    /// Describe guest RAM to the backend, including the fd it must map.
    /// Hand the backend a channel it can send mapping requests on, and serve
    /// it until the connection closes.
    fn start_backend_requests(&mut self) -> Result<()> {
        let window = self
            .window
            .clone()
            .context("a request channel was negotiated without a window")?;

        let mut handler =
            FrontendReqHandler::new(window).context("creating the backend request channel")?;
        // Without this the handler never sends the acknowledgement, while the
        // backend -- which negotiated REPLY_ACK on the same connection -- sets
        // need-reply on every request and blocks waiting for one. The symptom
        // is not a protocol error but a hang, and then a torn stream.
        handler.set_reply_ack_flag(true);
        self.frontend
            .set_backend_request_fd(&handler.get_tx_raw_fd())
            .context("VHOST_USER_SET_BACKEND_REQ_FD")?;

        // Spawned from the I/O set, because this runs on whichever vCPU thread
        // wrote DRIVER_OK and a thread is born with its parent's CPUs and
        // cgroup. Left on a pinned vCPU's CPU, every mapping request would wait
        // for the host to take that CPU from the guest -- which, dedicated, it
        // never gives back by halting -- while the backend blocks on the reply
        // and the guest's map ioctl blocks on the backend.
        crate::affinity::with_io_affinity("nvgpu-window", || {
            std::thread::Builder::new()
                .name("nvgpu-window".into())
                .spawn(move || {
                    loop {
                        match handler.handle_request() {
                            Ok(_) => {}
                            Err(e) => {
                                // The backend closing is an ordinary shutdown,
                                // not a fault; anything else is worth a line.
                                log::debug!("nvgpu window request channel closed: {e}");
                                break;
                            }
                        }
                    }
                })
        })
        .context("spawning the window request thread")?;
        Ok(())
    }

    fn memory_regions(&self) -> Result<Vec<VhostUserMemoryRegionInfo>> {
        self.mem
            .iter()
            .map(|region| {
                let file_offset = region.file_offset().context(
                    "guest memory is not file-backed; vhost-user backends must be able to map it",
                )?;
                let host_addr = self
                    .mem
                    .get_host_address(region.start_addr())
                    .context("no host address for RAM region")?;
                Ok(VhostUserMemoryRegionInfo {
                    guest_phys_addr: region.start_addr().raw_value(),
                    memory_size: region.len(),
                    userspace_addr: host_addr as u64,
                    mmap_offset: file_offset.start(),
                    mmap_handle: file_offset.file().as_raw_fd(),
                })
            })
            .collect()
    }

    fn reset(&mut self) {
        if self.running {
            for idx in 0..NUM_QUEUES {
                let _ = self.frontend.set_vring_enable(idx, false);
            }
            self.running = false;
        }
        self.queues = new_queues();
        self.qs = 0;
        // A reset guest has no vma left over any pool.
        if let Some(w) = &self.window {
            w.withdraw_all_pools();
        }
    }

    fn sq(&self) -> &QState {
        &self.queues[(self.qs as usize).min(NUM_QUEUES - 1)]
    }

    fn sqm(&mut self) -> &mut QState {
        &mut self.queues[(self.qs as usize).min(NUM_QUEUES - 1)]
    }
}

fn new_queues() -> [QState; NUM_QUEUES] {
    std::array::from_fn(|i| QState {
        size: QUEUE_SIZE,
        vec: i as u16,
        ..Default::default()
    })
}

/// The host GPU driver version, as its procfs reports it.
///
/// Matched by shape rather than by field position: the wording around the
/// number differs between driver builds and has changed before, but a bare
/// three-part dotted number in that line has not.
fn driver_version(proc_root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(proc_root.join("version")).ok()?;
    text.split_whitespace()
        .find(|w| {
            let mut parts = w.split('.');
            let num = |p: Option<&str>| {
                p.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            };
            num(parts.next()) && num(parts.next()) && num(parts.next()) && parts.next().is_none()
        })
        .map(str::to_string)
}

/// Check a config the backend served before any guest reads it.
///
/// The guest driver trusts these fields to index fixed arrays, and it reports a
/// bad one as a failed probe with no detail. So the bounds it relies on are
/// checked here, where the reason can be given. The driver version must also be
/// the module this host has loaded: the guest's userspace is staged from this
/// host, and a backend describing some other driver is talking to a different
/// host or a fixture.
///
/// The video memory limit is the backend's to enforce, since only it sees the
/// guest's allocations; it is a flag on a process this one did not start. So
/// the limit this VMM was configured with must be the one the backend
/// announces, or the guest would run with a limit nobody is enforcing.
fn check_device_config(cfg: &[u8], host_version: &str, vram_limit_mib: Option<u64>) -> Result<()> {
    anyhow::ensure!(
        cfg.len() == CONFIG_LEN,
        "the backend served {} bytes of device config; the guest driver reads {CONFIG_LEN}",
        cfg.len()
    );
    let u32_at = |off: usize| u32::from_le_bytes(cfg[off..off + 4].try_into().expect("4 bytes"));
    let gpus = u32_at(OFF_NUM_GPUS);
    anyhow::ensure!(
        (1..=MAX_GPUS).contains(&gpus),
        "the backend describes {gpus} GPUs; the guest driver takes 1 to {MAX_GPUS}"
    );
    let fds = u32_at(OFF_NUM_FD_TRANSLATIONS);
    anyhow::ensure!(
        fds <= MAX_FD_TRANSLATIONS,
        "the backend lists {fds} descriptor-carrying ioctls; the guest driver takes {MAX_FD_TRANSLATIONS}"
    );
    let version = &cfg[..32];
    let end = version
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(version.len());
    let version = std::str::from_utf8(&version[..end]).unwrap_or("");
    anyhow::ensure!(
        version == host_version,
        "the backend describes driver {version:?} but this host has {host_version} loaded"
    );
    let announced = u64::from_le_bytes(
        cfg[OFF_VRAM_LIMIT_MIB..OFF_VRAM_LIMIT_MIB + 8]
            .try_into()
            .expect("8 bytes"),
    );
    match vram_limit_mib {
        Some(want) => anyhow::ensure!(
            announced == want,
            "configured with a {want} MiB video memory limit, but the backend {}; \
             start it with --vram-limit-mib {want}",
            if announced == 0 {
                "enforces none".to_string()
            } else {
                format!("enforces {announced} MiB")
            }
        ),
        None if announced != 0 => log::info!(
            "virtio-gpu-nv: no video memory limit configured here; the backend enforces \
             {announced} MiB"
        ),
        None => {
            log::warn!("virtio-gpu-nv: no video memory limit; this guest may take the whole card")
        }
    }
    log::info!(
        "virtio-gpu-nv: driver {version}, {gpus} GPU(s), {fds} descriptor-carrying ioctl(s), \
         capabilities {:#x}",
        u32_at(36)
    );
    Ok(())
}

pub struct NvGpuDevice {
    inner: Mutex<Inner>,
}

impl NvGpuDevice {
    /// Connect to a forwarding backend already listening on `socket_path`.
    pub fn new(
        socket_path: &Path,
        proc_root: &Path,
        vram_limit_mib: Option<u64>,
        mem: Arc<GuestMemoryMmap>,
    ) -> Result<Self> {
        // Checked before connecting: a host with no driver loaded cannot have a
        // backend worth talking to, and the reason is clearer here.
        let host_version = driver_version(proc_root).with_context(|| {
            format!(
                "no GPU driver version under {}; is the host kernel module loaded?",
                proc_root.display()
            )
        })?;

        let mut frontend =
            Frontend::connect(socket_path, NUM_QUEUES as u64).with_context(|| {
                format!(
                    "failed to connect to the GPU forwarding backend at {}",
                    socket_path.display()
                )
            })?;
        let device_config = Self::fetch_device_config(&mut frontend)?;
        check_device_config(&device_config, &host_version, vram_limit_mib)?;

        let kick_fds = (0..NUM_QUEUES)
            .map(|_| {
                EventFd::new(0)
                    .map(Arc::new)
                    .context("failed to create virtio-gpu-nv kick eventfd")
            })
            .collect::<Result<Vec<_>>>()?;

        let (cfg, msix_cap) = Self::build_pci_config();
        log::info!(
            "virtio-gpu-nv: backend {} ({CONFIG_LEN}-byte config)",
            socket_path.display()
        );
        Ok(Self {
            inner: Mutex::new(Inner {
                window: None,
                com: ComCfg::default(),
                qs: 0,
                queues: new_queues(),
                isr: 0,
                cfg_vec: VIRTQ_MSI_NO_VECTOR,
                mem,
                msix: MsixTable::default(),
                cfg,
                msix_cap,
                frontend,
                kick_fds,
                running: false,
                device_config,
            }),
        })
    }

    /// Ask the backend for the guest-visible device configuration.
    ///
    /// This is the start of the vhost-user handshake: SET_OWNER, then just
    /// enough protocol negotiation for GET_CONFIG. The backend accepts
    /// SET_PROTOCOL_FEATURES before SET_FEATURES, and `activate` negotiates
    /// again in full once the guest has chosen its features.
    fn fetch_device_config(frontend: &mut Frontend) -> Result<Vec<u8>> {
        frontend.set_owner().context("VHOST_USER_SET_OWNER")?;
        let features = frontend.get_features().context("VHOST_USER_GET_FEATURES")?;
        anyhow::ensure!(
            features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() != 0,
            "the backend offers no vhost-user protocol features, so it cannot describe the device"
        );
        let offered = frontend
            .get_protocol_features()
            .context("VHOST_USER_GET_PROTOCOL_FEATURES")?;
        anyhow::ensure!(
            offered.contains(VhostUserProtocolFeatures::CONFIG),
            "the backend does not serve VHOST_USER_GET_CONFIG, so it cannot describe the device"
        );
        frontend
            .set_protocol_features(
                offered
                    & (VhostUserProtocolFeatures::MQ
                        | VhostUserProtocolFeatures::REPLY_ACK
                        | VhostUserProtocolFeatures::CONFIG),
            )
            .context("VHOST_USER_SET_PROTOCOL_FEATURES")?;
        let get = |frontend: &mut Frontend, len: usize| {
            frontend
                .get_config(
                    0,
                    len as u32,
                    VhostUserConfigFlags::empty(),
                    &vec![0u8; len],
                )
                .map(|(_, payload)| payload.to_vec())
        };
        // A backend answers a read longer than its config with an empty
        // reply, which arrives as an error on a connection still usable. A
        // backend from before the video memory limit is asked again for what
        // it has, and the field it lacks is zero: no limit, which the check
        // above refuses if one was configured.
        match get(frontend, CONFIG_LEN) {
            Ok(cfg) => Ok(cfg),
            Err(e) => {
                let mut cfg = get(frontend, CONFIG_LEN_NO_VRAM_LIMIT)
                    .map_err(|_| e)
                    .context("VHOST_USER_GET_CONFIG")?;
                cfg.resize(CONFIG_LEN, 0);
                Ok(cfg)
            }
        }
    }

    /// Give the device somewhere to put the mappings the backend asks for.
    ///
    /// Without this the backend is never offered a request channel, and every
    /// mapping stays in its own address space where the guest cannot reach it.
    pub fn bind_mapper(&self, mapper: Arc<dyn HostMemoryMapper>) {
        self.inner.lock().unwrap().window = Some(Arc::new(WindowMapper {
            mapper,
            guest_base: Mutex::new(None),
            aperture_base: Mutex::new(None),
            pools: Mutex::new(Default::default()),
        }));
    }

    /// Tell the device where the bus put BAR 2, which it cannot know earlier.
    pub fn set_shm_guest_addr(&self, addr: u64) {
        if let Some(w) = &self.inner.lock().unwrap().window {
            *w.guest_base.lock().unwrap() = Some(addr);
        }
    }

    /// Tell the device where the bus put BAR 4, the UVM aperture.
    pub fn set_aperture_guest_addr(&self, addr: u64) {
        if let Some(w) = &self.inner.lock().unwrap().window {
            *w.aperture_base.lock().unwrap() = Some(addr);
        }
    }

    /// Which BAR the aperture is.
    pub fn aperture_bar() -> usize {
        APERTURE_BAR
    }

    /// The window's size, for the caller that has to reserve it.
    pub fn shm_bar_size() -> u64 {
        SHM_SIZE
    }

    /// Which BAR the window is, for the caller that has to ask the bus for its
    /// address.
    pub fn shm_bar() -> usize {
        SHM_BAR
    }

    pub fn bind_interrupts(
        &self,
        vectors: Vec<MsiVector>,
        router: Arc<dyn MsiRouter>,
        intx: Arc<EventFd>,
    ) {
        self.inner.lock().unwrap().msix.bind(vectors, router, intx);
    }

    fn build_pci_config() -> ([u8; 256], u16) {
        // Class 0x038000: display controller, other.
        let mut cfg = PciConfig::new(
            0x1AF4,
            PCI_DEVICE_ID,
            0x01,
            0x03_80_00,
            0x1AF4,
            VIRTIO_ID_GPU_NV,
        );
        cfg.set_bar_mem(0, NV_BAR0_SIZE);
        cfg.set_irq_pin(1);
        cfg.add_virtio_cap(1, 0, NV_OFF_COMMON as u32, 0x38);
        cfg.add_virtio_notify_cap(0, NV_OFF_NOTIFY as u32, 0x100, NOTIFY_MULT);
        cfg.add_virtio_cap(3, 0, NV_OFF_ISR as u32, 1);
        cfg.add_virtio_cap(4, 0, NV_OFF_DEVICE as u32, CONFIG_LEN as u32);
        cfg.set_bar_mem64(SHM_BAR, SHM_SIZE);
        cfg.add_virtio_shm_cap(NV_SHM_ID, SHM_BAR as u8, 0, SHM_SIZE);
        cfg.set_bar_mem64(APERTURE_BAR, APERTURE_SIZE);
        cfg.add_virtio_shm_cap(NV_SHM_ID_APERTURE, APERTURE_BAR as u8, 0, APERTURE_SIZE);
        let msix_cap = cfg.add_msix_cap(
            MSIX_VECTORS - 1,
            NV_OFF_MSIX_TABLE as u32,
            NV_OFF_MSIX_PBA as u32,
        );
        cfg.add_pcie_cap(PCIE_TYPE_RC_INTEGRATED);
        (cfg.build(), msix_cap)
    }

    fn com_read(&self, off: u64, d: &mut [u8]) {
        let i = self.inner.lock().unwrap();
        let q = i.sq();
        let v = com_read(
            &i.com,
            off,
            i.features(),
            NUM_QUEUES as u64,
            i.cfg_vec as u64,
            i.qs as u64,
            q.size as u64,
            q.vec as u64,
            q.enabled as u64,
            i.qs as u64,
            q.desc & 0xFFFF_FFFF,
            q.desc >> 32,
            q.avail & 0xFFFF_FFFF,
            q.avail >> 32,
            q.used & 0xFFFF_FFFF,
            q.used >> 32,
        );
        write_val(d, v);
    }

    fn com_write(&self, off: u64, d: &[u8]) {
        let (v3, v2, v1) = parse_write(d);
        let mut i = self.inner.lock().unwrap();
        match off {
            CFG_DEVICE_FEAT_SEL => i.com.dfs = v3,
            CFG_DRIVER_FEAT_SEL => i.com.dff = v3,
            CFG_DRIVER_FEAT => write_driver_feature(&mut i.com, v3),
            CFG_MSIX_CONFIG => i.cfg_vec = v2,
            CFG_STATUS => {
                i.com.st = v1;
                if v1 == 0 {
                    i.reset();
                } else if v1 & STATUS_DRIVER_OK != 0 {
                    if let Err(err) = i.activate() {
                        log::error!("failed to start virtio-gpu-nv: {err:#}");
                    }
                }
            }
            CFG_QUEUE_SEL => i.qs = v2,
            CFG_QUEUE_SIZE => set_queue_size(i.sqm(), v2, QUEUE_SIZE),
            CFG_QUEUE_MSIX => i.sqm().vec = v2,
            CFG_QUEUE_ENABLE => i.sqm().enabled = v2 != 0,
            _ => write_queue_addr(i.sqm(), off, v3),
        }
    }

    /// Serve a device-config read out of the cached description.
    ///
    /// The guest chooses offset and length, so a read that starts or ends past
    /// the end is answered with zeros rather than trusted.
    fn device_config_read(&self, off: u64, d: &mut [u8]) {
        let i = self.inner.lock().unwrap();
        let start = off as usize;
        if start >= i.device_config.len() {
            d.fill(0);
            return;
        }
        let avail = i.device_config.len() - start;
        let n = d.len().min(avail);
        d[..n].copy_from_slice(&i.device_config[start..start + n]);
        d[n..].fill(0);
    }

    fn bar0_read(&self, o: u64, d: &mut [u8]) {
        if o < NV_OFF_ISR {
            self.com_read(o - NV_OFF_COMMON, d);
        } else if o < NV_OFF_NOTIFY {
            let mut i = self.inner.lock().unwrap();
            if !d.is_empty() {
                d[0] = i.isr;
                i.isr = 0;
            }
        } else if o < NV_OFF_MSIX_TABLE {
            d.fill(0);
        } else if o < NV_OFF_MSIX_PBA {
            self.inner
                .lock()
                .unwrap()
                .msix
                .read(o - NV_OFF_MSIX_TABLE, d);
        } else if o < NV_OFF_DEVICE {
            self.inner
                .lock()
                .unwrap()
                .msix
                .read_pba(o - NV_OFF_MSIX_PBA, d);
        } else if o < NV_BAR0_SIZE {
            self.device_config_read(o - NV_OFF_DEVICE, d);
        } else {
            d.fill(0);
        }
    }

    fn bar0_write(&self, o: u64, d: &[u8]) {
        if o < NV_OFF_ISR {
            self.com_write(o - NV_OFF_COMMON, d);
        } else if o < NV_OFF_NOTIFY {
            // ISR is read-to-clear.
        } else if o < NV_OFF_MSIX_TABLE {
            let idx = ((o - NV_OFF_NOTIFY) / NOTIFY_MULT as u64) as usize;
            let i = self.inner.lock().unwrap();
            if let Some(fd) = i.kick_fds.get(idx) {
                let _ = fd.write(1);
            }
        } else if o < NV_OFF_MSIX_PBA {
            let mut i = self.inner.lock().unwrap();
            if i.msix.write(o - NV_OFF_MSIX_TABLE, d) {
                i.msix
                    .trigger_unmasked(((o - NV_OFF_MSIX_TABLE) / 16) as usize);
            }
        }
        // Device config is read-only to the guest.
    }
}

impl PciDevice for NvGpuDevice {
    /// A guest kick goes straight to the eventfd the backend waits on, with no
    /// exit to userspace. `bar0_write` still handles the same offsets for a
    /// host where registration fails.
    fn doorbells(&self) -> Vec<Doorbell> {
        let i = self.inner.lock().unwrap();
        i.kick_fds
            .iter()
            .enumerate()
            .map(|(idx, fd)| Doorbell {
                bar_idx: 0,
                offset: NV_OFF_NOTIFY + idx as u64 * NOTIFY_MULT as u64,
                fd: fd.clone(),
            })
            .collect()
    }
    fn read_config(&self, o: u32, d: &mut [u8]) {
        let i = self.inner.lock().unwrap();
        read_cfg_space(&i.cfg, o, d);
    }
    fn write_config(&self, o: u32, d: &[u8]) {
        let mut i = self.inner.lock().unwrap();
        let cap = i.msix_cap;
        write_msix_control(&mut i.cfg, cap, o, d);
        i.msix.enabled = msix_enabled(&i.cfg, cap);
    }
    fn read_bar(&self, bi: usize, o: u64, d: &mut [u8]) -> bool {
        if bi == 0 {
            self.bar0_read(o, d);
            true
        } else {
            // BAR 2 is backed by real memory and the guest reaches it without
            // trapping, so an access arriving here is to a page the backend has
            // not placed anything in.
            d.fill(0);
            bi == SHM_BAR || bi == APERTURE_BAR
        }
    }
    fn write_bar(&self, bi: usize, o: u64, d: &[u8]) -> bool {
        if bi == 0 {
            self.bar0_write(o, d);
            true
        } else {
            bi == SHM_BAR || bi == APERTURE_BAR
        }
    }
    fn bar_size(&self, bi: usize) -> u64 {
        match bi {
            0 => NV_BAR0_SIZE,
            SHM_BAR => SHM_SIZE,
            APERTURE_BAR => APERTURE_SIZE,
            _ => 0,
        }
    }
    fn bar_type(&self, bi: usize) -> BarType {
        if bi == SHM_BAR || bi == APERTURE_BAR {
            BarType::Mem64
        } else {
            BarType::Mem32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(version: &str, gpus: u32, fds: u32) -> Vec<u8> {
        config_with_vram(version, gpus, fds, 0)
    }

    fn config_with_vram(version: &str, gpus: u32, fds: u32, vram_mib: u64) -> Vec<u8> {
        let mut cfg = vec![0u8; CONFIG_LEN];
        cfg[OFF_VRAM_LIMIT_MIB..OFF_VRAM_LIMIT_MIB + 8].copy_from_slice(&vram_mib.to_le_bytes());
        cfg[..version.len()].copy_from_slice(version.as_bytes());
        cfg[OFF_NUM_GPUS..OFF_NUM_GPUS + 4].copy_from_slice(&gpus.to_le_bytes());
        cfg[OFF_NUM_FD_TRANSLATIONS..OFF_NUM_FD_TRANSLATIONS + 4]
            .copy_from_slice(&fds.to_le_bytes());
        cfg
    }

    #[test]
    fn a_well_formed_config_for_this_host_is_accepted() {
        assert!(check_device_config(&config("615.71.09", 1, 5), "615.71.09", None).is_ok());
    }

    /// The guest driver indexes fixed arrays with these, so each bound is
    /// checked rather than passed on.
    #[test]
    fn counts_the_guest_driver_cannot_index_are_refused() {
        assert!(check_device_config(&config("615.71.09", 0, 5), "615.71.09", None).is_err());
        assert!(check_device_config(&config("615.71.09", 9, 5), "615.71.09", None).is_err());
        assert!(check_device_config(&config("615.71.09", 1, 17), "615.71.09", None).is_err());
        assert!(
            check_device_config(&config("615.71.09", 1, 5)[..4000], "615.71.09", None).is_err()
        );
    }

    /// The limit is enforced by the backend, so a limit the backend does not
    /// announce is a limit nobody enforces.
    #[test]
    fn a_configured_vram_limit_must_be_the_one_the_backend_enforces() {
        let v = "615.71.09";
        assert!(check_device_config(&config_with_vram(v, 1, 5, 4096), v, Some(4096)).is_ok());
        let err = check_device_config(&config_with_vram(v, 1, 5, 0), v, Some(4096))
            .expect_err("backend enforces none");
        assert!(
            format!("{err:#}").contains("--vram-limit-mib 4096"),
            "{err:#}"
        );
        assert!(check_device_config(&config_with_vram(v, 1, 5, 2048), v, Some(4096)).is_err());
    }

    /// A backend stricter than this side asked for is no risk.
    #[test]
    fn a_backend_limit_with_none_configured_here_is_accepted() {
        let v = "615.71.09";
        assert!(check_device_config(&config_with_vram(v, 1, 5, 2048), v, None).is_ok());
        assert!(check_device_config(&config(v, 1, 5), v, None).is_ok());
    }

    #[test]
    fn a_backend_describing_another_driver_is_refused_with_both_versions() {
        let err = check_device_config(&config("595.104.02", 1, 5), "615.71.09", None)
            .expect_err("versions differ");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("595.104.02") && msg.contains("615.71.09"),
            "{msg}"
        );
    }

    #[test]
    fn the_driver_version_is_read_from_procfs_by_shape() {
        let dir = std::env::temp_dir().join(format!("nvgpu-ver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("version"),
            "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  615.71.09  Release Build\n",
        )
        .expect("write");
        assert_eq!(driver_version(&dir).as_deref(), Some("615.71.09"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A guest maps device config with PAGE_SIZE as its maximum and truncates
    /// the rest without saying so, so this is not a budget but a wall.
    #[test]
    fn config_fits_in_one_page() {
        assert!(
            CONFIG_LEN <= 4096,
            "config is {CONFIG_LEN} bytes; a guest cannot read past 4096"
        );
    }

    #[test]
    fn pci_identity_matches_the_guest_driver() {
        assert_eq!(VIRTIO_ID_GPU_NV, 45);
        assert_eq!(PCI_DEVICE_ID, 0x106D);
    }

    /// The common layout leaves 256 bytes for device config. This one needs
    /// 8912, which is why it carries its own offsets.
    #[test]
    fn device_config_fits_in_the_bar_with_room_to_grow() {
        assert!(NV_OFF_DEVICE + CONFIG_LEN as u64 <= NV_BAR0_SIZE);
        assert!(
            CONFIG_LEN > (OFF_NOTIFY - OFF_DEVICE) as usize,
            "if config now fits the shared layout, this device should use it"
        );
    }

    /// Every region must start after the one before it and none may overlap
    /// config, or a guest read lands in the wrong one and the failure looks
    /// like corrupt data rather than a layout mistake.
    #[test]
    fn bar_regions_are_ordered_and_disjoint() {
        let bounds = [
            NV_OFF_COMMON,
            NV_OFF_ISR,
            NV_OFF_NOTIFY,
            NV_OFF_MSIX_TABLE,
            NV_OFF_MSIX_PBA,
            NV_OFF_DEVICE,
            NV_BAR0_SIZE,
        ];
        for pair in bounds.windows(2) {
            assert!(pair[0] < pair[1], "region bounds {pair:?} are out of order");
        }
    }

    #[test]
    fn there_is_an_interrupt_for_every_queue_and_the_config() {
        assert_eq!(MSIX_VECTORS as usize, NUM_QUEUES + 1);
    }

    /// The driver calls virtio_find_vqs(vdev, 2, ...) and fails probe on that
    /// call's error, so this is the number that has to hold.
    #[test]
    fn uvm_pools_are_checked_before_placement() {
        const MIB: u64 = 1 << 20;
        let rw = VhostUserMMapFlags::WRITABLE.bits();
        let hva = 8u64 << 30;
        let mut pools = std::collections::BTreeMap::new();
        assert_eq!(check_pool(&pools, hva, 0, 2 * MIB, rw), Ok(()));
        pools.insert(0, (2 * MIB, hva));
        for (h, o, l, f) in [
            (hva + 4 * MIB, 4 * MIB, 2 * MIB, 0),
            (hva + 4 * MIB, 4 * MIB, 0, rw),
            (hva + 4 * MIB, 4 * MIB, 4097, rw),
            (hva + 4 * MIB + 1, 4 * MIB, 4096, rw),
            (hva + 4 * MIB, 4 * MIB + 4096, 4096, rw),
            (hva + 4 * MIB, 4 * MIB, POOL_MAX_LEN + 4096, rw),
            (POOL_HVA_MIN - 4096, 4 * MIB, 4096, rw),
            (POOL_HVA_MAX - 4096, 4 * MIB, 8192, rw),
            (hva + 4 * MIB, APERTURE_SIZE - 2 * MIB, 4 * MIB, rw),
        ] {
            assert_eq!(
                check_pool(&pools, h, o, l, f),
                Err(libc::EINVAL),
                "{h:#x} {o:#x} {l:#x}"
            );
        }
        assert_eq!(
            check_pool(&pools, hva + 4 * MIB, 0, MIB, rw),
            Err(libc::EEXIST)
        );
        assert_eq!(
            check_pool(&pools, hva + MIB, 4 * MIB, MIB, rw),
            Err(libc::EEXIST)
        );
        for i in 1..4u64 {
            pools.insert(i * 64 * MIB, (64 * MIB, hva + i * 64 * MIB));
        }
        assert_eq!(
            check_pool(&pools, hva + 512 * MIB, 512 * MIB, 64 * MIB, rw),
            Err(libc::ENOSPC)
        );
    }

    #[test]
    fn the_aperture_is_its_own_bar_with_its_own_id() {
        assert_ne!(APERTURE_BAR, SHM_BAR);
        assert_ne!(NV_SHM_ID_APERTURE, NV_SHM_ID);
        assert!(APERTURE_SIZE.is_power_of_two());
    }

    #[test]
    fn two_queues_are_offered() {
        assert_eq!(NUM_QUEUES, 2);
        assert_eq!(new_queues().len(), 2);
    }
}
