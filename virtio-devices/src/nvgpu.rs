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
//! **Device config is built here, not fetched.** The guest driver reads a
//! structure describing the host driver version and the GPUs it owns, at fixed
//! offsets. The obvious source is the backend, over `VHOST_USER_GET_CONFIG` --
//! but that message caps `offset + size` at 4096 bytes. It is therefore
//! assembled here from what the kernel publishes under `/proc/driver/nvidia`,
//! the same way virtio-fs builds its own tag config locally.
//!
//! Two independent 4 KiB limits meet here, which is worth knowing before
//! anyone tries to grow this structure: the vhost-user config message is one,
//! and a guest mapping device config with PAGE_SIZE as its maximum is the
//! other. Neither is in this code, and neither can be raised from it.
//!
//! **It cannot use the common BAR layout.** That layout leaves 256 bytes
//! between the ISR and notify regions for device config, which is 35 times too
//! small here. The offsets below are therefore local to this device. They are
//! ordered with the small regions first and config last, so config can grow
//! without moving anything else.

use crate::common::*;
use anyhow::{Context, Result};
use pci::config::{PCIE_TYPE_RC_INTEGRATED, PciConfig};
use pci::{MsiRouter, MsiVector, PciDevice};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::{Arc, Mutex};
use vhost::vhost_user::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};
use vhost::vhost_user::{Frontend, VhostUserFrontend};
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
const CONFIG_LEN: usize = 4016;

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
    kick_fds: Vec<EventFd>,
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

        self.frontend.set_owner().context("VHOST_USER_SET_OWNER")?;

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
            // Only the ones we implement: there is no backend-request channel
            // here, so BACKEND_REQ stays unacknowledged.
            let wanted = VhostUserProtocolFeatures::MQ | VhostUserProtocolFeatures::REPLY_ACK;
            self.frontend
                .set_protocol_features(offered & wanted)
                .context("VHOST_USER_SET_PROTOCOL_FEATURES")?;
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

pub struct NvGpuDevice {
    inner: Mutex<Inner>,
}

impl NvGpuDevice {
    /// Connect to a forwarding backend already listening on `socket_path`.
    pub fn new(socket_path: &Path, proc_root: &Path, mem: Arc<GuestMemoryMmap>) -> Result<Self> {
        // Built before connecting: if this host cannot describe a GPU there is
        // no point holding a backend socket open, and the reason is clearer
        // here than after a handshake.
        let device_config = Self::build_device_config(proc_root)?;

        let frontend = Frontend::connect(socket_path, NUM_QUEUES as u64).with_context(|| {
            format!(
                "failed to connect to the GPU forwarding backend at {}",
                socket_path.display()
            )
        })?;

        let kick_fds = (0..NUM_QUEUES)
            .map(|_| EventFd::new(0).context("failed to create virtio-gpu-nv kick eventfd"))
            .collect::<Result<Vec<_>>>()?;

        let (cfg, msix_cap) = Self::build_pci_config();
        log::info!(
            "virtio-gpu-nv: backend {} ({CONFIG_LEN}-byte config)",
            socket_path.display()
        );
        Ok(Self {
            inner: Mutex::new(Inner {
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

    /// Assemble the guest-visible device configuration.
    ///
    /// Every offset here is read by a guest driver that will not tell you when
    /// it disagrees: a wrong one shows up as a failed probe with no detail, so
    /// they are named as constants and asserted in tests rather than written
    /// inline.
    fn build_device_config(proc_root: &Path) -> Result<Vec<u8>> {
        const OFF_DRIVER_VERSION: usize = 0;
        const LEN_DRIVER_VERSION: usize = 32;
        const OFF_NUM_GPUS: usize = 32;
        const OFF_GPUS: usize = 72;
        const SLOT_LEN: usize = 476;
        const SLOT_PCI_ADDR: usize = 0;
        const LEN_PCI_ADDR: usize = 16;
        const SLOT_MINOR: usize = 16;
        const SLOT_INFO_LEN: usize = 20;
        const SLOT_INFO_TEXT: usize = 28;
        const LEN_INFO_TEXT: usize = 448;
        const MAX_GPUS: usize = 8;
        const OFF_NUM_FD_TRANSLATIONS: usize = 3880;
        const OFF_FD_TRANSLATIONS: usize = 3888;

        // Which ioctls carry a file descriptor, and where it sits in the
        // parameter struct. The guest driver rewrites a descriptor only for an
        // ioctl named here; for any other it forwards the guest's own fd
        // number, which means nothing on the host, and the backend refuses the
        // call. Publishing an empty table is therefore not a degraded mode: it
        // is the difference between nvidia-smi working and it reporting
        // "Unable to determine the device handle for GPU0".
        //
        // 201 NV_ESC_REGISTER_FD      the descriptor is the whole struct
        // 206 NV_ESC_ALLOC_OS_EVENT   after hClient and hDevice
        // 207 NV_ESC_FREE_OS_EVENT    same layout as alloc
        //  39 NV_ESC_RM_ALLOC_MEMORY
        const FD_CARRYING_IOCTLS: &[(u32, u32)] =
            &[(201, 0), (206, 8), (207, 8), (39, 48)];

        let version = driver_version(proc_root).with_context(|| {
            format!(
                "no GPU driver version under {}; is the host kernel module loaded?",
                proc_root.display()
            )
        })?;

        let mut addrs: Vec<String> = std::fs::read_dir(proc_root.join("gpus"))
            .with_context(|| format!("no GPUs listed under {}", proc_root.display()))?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        // readdir order is not stable, and a guest that sees its GPUs in a
        // different order across boots addresses the wrong card.
        addrs.sort();
        anyhow::ensure!(
            !addrs.is_empty(),
            "the host driver is loaded but owns no GPUs; the guest driver rejects an empty table"
        );

        let mut cfg = vec![0u8; CONFIG_LEN];
        let put = |cfg: &mut [u8], at: usize, bytes: &[u8], cap: usize| {
            let n = bytes.len().min(cap);
            cfg[at..at + n].copy_from_slice(&bytes[..n]);
            n
        };

        // One byte short of the field, so the terminator the guest writes over
        // the last byte lands on padding rather than on a character.
        put(
            &mut cfg,
            OFF_DRIVER_VERSION,
            version.as_bytes(),
            LEN_DRIVER_VERSION - 1,
        );

        let count = addrs.len().min(MAX_GPUS);
        if addrs.len() > MAX_GPUS {
            log::warn!(
                "host has {} GPUs; config space describes {MAX_GPUS}, so the rest are not offered",
                addrs.len()
            );
        }
        cfg[OFF_NUM_GPUS..OFF_NUM_GPUS + 4].copy_from_slice(&(count as u32).to_le_bytes());

        for (i, addr) in addrs.iter().take(count).enumerate() {
            let base = OFF_GPUS + i * SLOT_LEN;
            put(
                &mut cfg,
                base + SLOT_PCI_ADDR,
                addr.as_bytes(),
                LEN_PCI_ADDR - 1,
            );
            // The index is the minor only because the kernel numbers the device
            // nodes in the same order it lists these directories in.
            cfg[base + SLOT_MINOR..base + SLOT_MINOR + 4].copy_from_slice(&(i as u32).to_le_bytes());

            let info = std::fs::read_to_string(proc_root.join("gpus").join(addr).join("information"))
                .unwrap_or_default();
            let n = put(
                &mut cfg,
                base + SLOT_INFO_TEXT,
                info.as_bytes(),
                LEN_INFO_TEXT,
            );
            cfg[base + SLOT_INFO_LEN..base + SLOT_INFO_LEN + 4]
                .copy_from_slice(&(n as u32).to_le_bytes());
        }

        for (i, (nr, off)) in FD_CARRYING_IOCTLS.iter().enumerate() {
            let at = OFF_FD_TRANSLATIONS + i * 8;
            cfg[at..at + 4].copy_from_slice(&nr.to_le_bytes());
            cfg[at + 4..at + 8].copy_from_slice(&off.to_le_bytes());
        }
        cfg[OFF_NUM_FD_TRANSLATIONS..OFF_NUM_FD_TRANSLATIONS + 4]
            .copy_from_slice(&(FD_CARRYING_IOCTLS.len() as u32).to_le_bytes());

        log::info!(
            "virtio-gpu-nv: host driver {version}, {count} GPU(s), {} fd-translation ioctl(s)",
            FD_CARRYING_IOCTLS.len()
        );
        Ok(cfg)
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
            false
        }
    }
    fn write_bar(&self, bi: usize, o: u64, d: &[u8]) -> bool {
        if bi == 0 {
            self.bar0_write(o, d);
            true
        } else {
            false
        }
    }
    fn bar_size(&self, bi: usize) -> u64 {
        if bi == 0 { NV_BAR0_SIZE } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guest driver reads these at fixed offsets and rejects what it does
    /// not recognise, so they are a contract rather than a choice.
    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Self {
            let base = std::env::temp_dir().join(format!(
                "nvgpu-cfg-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).expect("mkdir");
            for (path, body) in files {
                let p = base.join(path);
                std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
                std::fs::write(&p, body).expect("write");
            }
            Self(base)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const VERSION_LINE: &str =
        "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  615.71.09  Release Build\n";

    fn u32_at(cfg: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(cfg[off..off + 4].try_into().expect("4 bytes"))
    }

    #[test]
    fn config_is_the_length_the_guest_driver_expects() {
        let f = Fixture::new(&[
            ("version", VERSION_LINE),
            ("gpus/0000:01:00.0/information", "Model: NVIDIA RTX A2000\n"),
        ]);
        let cfg = NvGpuDevice::build_device_config(f.path()).expect("config");
        assert_eq!(cfg.len(), CONFIG_LEN);
    }

    /// The offsets the driver reads by hand. A wrong one is a failed probe with
    /// no explanation, so each is checked against a known input.
    #[test]
    fn fields_land_where_the_guest_driver_reads_them() {
        let f = Fixture::new(&[
            ("version", VERSION_LINE),
            ("gpus/0000:01:00.0/information", "Model: NVIDIA RTX A2000\n"),
        ]);
        let cfg = NvGpuDevice::build_device_config(f.path()).expect("config");

        assert_eq!(&cfg[0..9], b"615.71.09", "driver version at offset 0");
        assert_eq!(u32_at(&cfg, 32), 1, "num_gpus at offset 32");
        assert_eq!(&cfg[72..84], b"0000:01:00.0", "first pci_addr at offset 72");
        assert_eq!(u32_at(&cfg, 72 + 16), 0, "minor at slot offset 16");
        assert_eq!(
            u32_at(&cfg, 72 + 20) as usize,
            "Model: NVIDIA RTX A2000\n".len(),
            "info_len at slot offset 20"
        );
        assert_eq!(&cfg[100..105], b"Model", "info_text at slot offset 28");
    }

    #[test]
    fn gpus_are_ordered_by_pci_address_not_readdir_order() {
        let f = Fixture::new(&[
            ("version", VERSION_LINE),
            ("gpus/0000:41:00.0/information", "C\n"),
            ("gpus/0000:01:00.0/information", "A\n"),
            ("gpus/0000:21:00.0/information", "B\n"),
        ]);
        let cfg = NvGpuDevice::build_device_config(f.path()).expect("config");
        assert_eq!(u32_at(&cfg, 32), 3);
        let addr = |i: usize| String::from_utf8_lossy(&cfg[72 + i * 476..72 + i * 476 + 12]).into_owned();
        assert_eq!(addr(0), "0000:01:00.0");
        assert_eq!(addr(1), "0000:21:00.0");
        assert_eq!(addr(2), "0000:41:00.0");
    }

    /// The guest driver rejects num_gpus == 0, so a host that cannot describe
    /// one must fail here, where the reason can be given.
    #[test]
    fn a_host_with_no_driver_is_refused_with_a_reason() {
        let empty = Fixture::new(&[]);
        let err = NvGpuDevice::build_device_config(empty.path())
            .expect_err("a host with no driver must not yield a config");
        assert!(
            format!("{err:#}").contains("driver version"),
            "unhelpful error: {err:#}"
        );

        let no_gpus = Fixture::new(&[("version", VERSION_LINE), ("gpus/.keep", "")]);
        assert!(NvGpuDevice::build_device_config(no_gpus.path()).is_err());
    }

    /// An information file longer than the window must be truncated, and the
    /// length written must match what was actually copied.
    #[test]
    fn an_over_long_information_file_is_truncated_honestly() {
        let big = "Model: X\n".repeat(500);
        let f = Fixture::new(&[
            ("version", VERSION_LINE),
            ("gpus/0000:01:00.0/information", big.as_str()),
        ]);
        let cfg = NvGpuDevice::build_device_config(f.path()).expect("config");
        assert_eq!(u32_at(&cfg, 72 + 20), 448, "info_len must match the copy");
        // The next slot must not have been written into.
        assert_eq!(&cfg[72 + 476..72 + 476 + 4], &[0, 0, 0, 0]);
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
    fn two_queues_are_offered() {
        assert_eq!(NUM_QUEUES, 2);
        assert_eq!(new_queues().len(), 2);
    }
}
