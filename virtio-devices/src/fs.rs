//! Virtio-fs device over PCI transport, backed by a vhost-user daemon.
//!
//! Unlike vsock, whose backend is a kernel module, virtio-fs is served by a
//! separate process (virtiofsd) over a Unix socket. We emulate the transport
//! and hand it the virtqueues; it does the FUSE work and reads and writes
//! guest memory directly, which is why guest RAM has to be shared memory.

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

const QUEUE_SIZE: u16 = 1024;
/// One high-priority queue plus one request queue.
const NUM_QUEUES: usize = 2;
const NUM_REQUEST_QUEUES: u32 = 1;
/// One per queue plus a config vector.
const MSIX_VECTORS: u16 = 4;

/// virtio device type 26; PCI device id is 0x1040 + type.
const VIRTIO_ID_FS: u16 = 26;
const PCI_DEVICE_ID: u16 = 0x1040 + VIRTIO_ID_FS;

/// Length of the tag field in the device config, NUL-padded.
const TAG_LEN: usize = 36;
/// tag + num_request_queues.
const CONFIG_LEN: usize = TAG_LEN + 4;

struct Inner {
    tag: [u8; TAG_LEN],
    com: ComCfg,
    qs: u16,
    queues: [QState; NUM_QUEUES],
    isr: u8,
    cfg_vec: u16,
    mem: Arc<GuestMemoryMmap>,
    msix: MsixTable<4>,
    cfg: [u8; 256],
    msix_cap: u16,
    frontend: Frontend,
    kick_fds: Vec<EventFd>,
    running: bool,
}

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
            // Only the ones we actually implement: we have no backend-request
            // channel, so BACKEND_REQ stays unacknowledged.
            let wanted = VhostUserProtocolFeatures::MQ
                | VhostUserProtocolFeatures::REPLY_ACK
                | VhostUserProtocolFeatures::CONFIG;
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
            // guest's: the memory table tells the backend how to map them
            // back. This is the opposite of the kernel backend, which takes
            // guest physical addresses and translates them itself.
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
        log::info!(
            "virtio-fs \"{}\" active",
            String::from_utf8_lossy(&self.tag).trim_end_matches('\0')
        );
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

pub struct FsDevice {
    inner: Mutex<Inner>,
}

impl FsDevice {
    /// Connect to a virtiofsd already listening on `socket_path`.
    pub fn new(tag: &str, socket_path: &Path, mem: Arc<GuestMemoryMmap>) -> Result<Self> {
        anyhow::ensure!(
            tag.len() < TAG_LEN,
            "virtio-fs tag must be shorter than {TAG_LEN} bytes"
        );
        let mut tag_bytes = [0u8; TAG_LEN];
        tag_bytes[..tag.len()].copy_from_slice(tag.as_bytes());

        let frontend = Frontend::connect(socket_path, NUM_QUEUES as u64).with_context(|| {
            format!(
                "failed to connect to virtiofsd at {}",
                socket_path.display()
            )
        })?;
        let kick_fds = (0..NUM_QUEUES)
            .map(|_| EventFd::new(0).context("failed to create virtio-fs kick eventfd"))
            .collect::<Result<Vec<_>>>()?;

        let (cfg, msix_cap) = Self::build_pci_config();
        log::info!("virtio-fs: tag \"{tag}\" via {}", socket_path.display());
        Ok(Self {
            inner: Mutex::new(Inner {
                tag: tag_bytes,
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
            }),
        })
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
        // Class 0x018000: mass storage, other.
        let mut cfg = PciConfig::new(
            0x1AF4,
            PCI_DEVICE_ID,
            0x01,
            0x01_80_00,
            0x1AF4,
            VIRTIO_ID_FS,
        );
        cfg.set_bar_mem(0, BAR0_SIZE);
        cfg.set_irq_pin(1);
        cfg.add_virtio_cap(1, 0, OFF_COMMON as u32, 0x38);
        cfg.add_virtio_notify_cap(0, OFF_NOTIFY as u32, 0x100, NOTIFY_MULT);
        cfg.add_virtio_cap(3, 0, OFF_ISR as u32, 1);
        cfg.add_virtio_cap(4, 0, OFF_DEVICE as u32, CONFIG_LEN as u32);
        let msix_cap =
            cfg.add_msix_cap(MSIX_VECTORS - 1, OFF_MSIX_TABLE as u32, OFF_MSIX_PBA as u32);
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
            CFG_DRIVER_FEAT => {
                if i.com.dff == 0 {
                    i.com.df = (i.com.df & 0xFFFF_FFFF_0000_0000) | (v3 as u64)
                } else {
                    i.com.df = (i.com.df & 0xFFFF_FFFF) | ((v3 as u64) << 32)
                }
            }
            CFG_MSIX_CONFIG => i.cfg_vec = v2,
            CFG_STATUS => {
                i.com.st = v1;
                if v1 == 0 {
                    i.reset();
                } else if v1 & STATUS_DRIVER_OK != 0 {
                    if let Err(err) = i.activate() {
                        log::error!("failed to start virtio-fs: {err:#}");
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

    fn bar0_read(&self, o: u64, d: &mut [u8]) {
        if o < OFF_ISR {
            self.com_read(o - OFF_COMMON, d);
        } else if o < OFF_DEVICE {
            let mut i = self.inner.lock().unwrap();
            if !d.is_empty() {
                d[0] = i.isr;
                i.isr = 0;
            }
        } else if o < OFF_NOTIFY {
            let i = self.inner.lock().unwrap();
            let mut config = [0u8; CONFIG_LEN];
            config[..TAG_LEN].copy_from_slice(&i.tag);
            config[TAG_LEN..].copy_from_slice(&NUM_REQUEST_QUEUES.to_le_bytes());
            let s = (o - OFF_DEVICE) as usize;
            let e = (s + d.len()).min(CONFIG_LEN);
            if s < CONFIG_LEN {
                d[..e - s].copy_from_slice(&config[s..e]);
                d[e - s..].fill(0);
            } else {
                d.fill(0);
            }
        } else if o < OFF_MSIX_TABLE {
            d.fill(0);
        } else if o < OFF_MSIX_PBA {
            self.inner.lock().unwrap().msix.read(o - OFF_MSIX_TABLE, d);
        } else if o < BAR0_SIZE {
            self.inner
                .lock()
                .unwrap()
                .msix
                .read_pba(o - OFF_MSIX_PBA, d);
        } else {
            d.fill(0);
        }
    }

    fn bar0_write(&self, o: u64, d: &[u8]) {
        if o < OFF_ISR {
            self.com_write(o - OFF_COMMON, d);
        } else if o < OFF_NOTIFY {
            // ISR and device config are read-only here.
        } else if o < OFF_MSIX_TABLE {
            let idx = ((o - OFF_NOTIFY) / NOTIFY_MULT as u64) as usize;
            let i = self.inner.lock().unwrap();
            if let Some(fd) = i.kick_fds.get(idx) {
                let _ = fd.write(1);
            }
        } else if o < OFF_MSIX_PBA {
            let mut i = self.inner.lock().unwrap();
            if i.msix.write(o - OFF_MSIX_TABLE, d) {
                i.msix
                    .trigger_unmasked(((o - OFF_MSIX_TABLE) / 16) as usize);
            }
        }
    }
}

impl PciDevice for FsDevice {
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
        if bi == 0 { BAR0_SIZE } else { 0 }
    }
}
