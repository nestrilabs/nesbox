//! Virtio vsock device over PCI transport, backed by the host's vhost-vsock.
//!
//! We emulate only the transport: config space, feature negotiation and the
//! virtqueue addresses. The queues themselves are handed to `/dev/vhost-vsock`,
//! which moves packets in the kernel. Once activated, guest notifications
//! reach the kernel through a kick eventfd and completions come back through
//! the queue's MSI-X eventfd — neither path re-enters this process.

use crate::common::*;
use anyhow::{Context, Result};
use pci::config::{PCIE_TYPE_RC_INTEGRATED, PciConfig};
use pci::{MsiRouter, MsiVector, PciDevice};
use std::sync::{Arc, Mutex};
use vhost::vhost_kern::vsock::Vsock as VhostVsockBackend;
use vhost::vsock::VhostVsock;
use vhost::{VhostBackend, VringConfigData};
use vm_memory::{Address, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};
use vmm_sys_util::eventfd::EventFd;

const QUEUE_SIZE: u16 = 256;
/// rx, tx, event.
const NUM_QUEUES: usize = 3;
/// Of those, the kernel backend owns only rx and tx. The event queue is used
/// for transport-reset notifications and stays with us; vhost-vsock rejects
/// any attempt to hand it a third vring.
const VHOST_QUEUES: usize = 2;
/// One per queue plus a config vector.
const MSIX_VECTORS: u16 = 4;

/// virtio device type 19; PCI device id is 0x1040 + type.
const VIRTIO_ID_VSOCK: u16 = 19;
const PCI_DEVICE_ID: u16 = 0x1040 + VIRTIO_ID_VSOCK;

/// The well-known CID of the host.
pub const VMADDR_CID_HOST: u64 = 2;

struct Inner {
    cid: u64,
    com: ComCfg,
    qs: u16,
    queues: [QState; NUM_QUEUES],
    isr: u8,
    cfg_vec: u16,
    mem: Option<Arc<GuestMemoryMmap>>,
    msix: MsixTable<4>,
    cfg: [u8; 256],
    msix_cap: u16,
    backend: VhostVsockBackend<Arc<GuestMemoryMmap>>,
    /// One kick eventfd per queue, signalled when the guest notifies.
    kick_fds: Vec<EventFd>,
    running: bool,
}

impl Inner {
    fn features(&self) -> u64 {
        // Keep the negotiated set minimal: the ring layout the backend and the
        // guest must agree on, and nothing optional.
        VIRTIO_F_VERSION_1
    }

    /// Hand the queues to the kernel and start moving packets.
    ///
    /// Called when the driver sets DRIVER_OK, by which point it has programmed
    /// every queue address and its MSI-X vectors.
    fn activate(&mut self) -> Result<()> {
        if self.running {
            return Ok(());
        }
        let mem = self.mem.clone().context("guest memory not attached")?;

        self.backend.set_owner().context("VHOST_SET_OWNER")?;

        let acked = self.com.df & self.features();
        self.backend
            .set_features(acked)
            .context("VHOST_SET_FEATURES")?;

        let regions: Vec<_> = mem
            .iter()
            .map(|region| {
                let host_addr = mem
                    .get_host_address(region.start_addr())
                    .context("no host address for RAM region")?;
                Ok(vhost::VhostUserMemoryRegionInfo {
                    guest_phys_addr: region.start_addr().raw_value(),
                    memory_size: region.len(),
                    userspace_addr: host_addr as u64,
                    mmap_offset: 0,
                    mmap_handle: -1,
                })
            })
            .collect::<Result<_>>()?;
        self.backend
            .set_mem_table(&regions)
            .context("VHOST_SET_MEM_TABLE")?;

        for (idx, queue) in self.queues.iter().take(VHOST_QUEUES).enumerate() {
            // The backend takes guest physical addresses and translates them
            // itself; handing it host addresses fails validation.
            anyhow::ensure!(
                queue.desc != 0 && queue.avail != 0 && queue.used != 0,
                "queue {idx} was never programmed by the driver"
            );

            self.backend
                .set_vring_num(idx, queue.size)
                .context("VHOST_SET_VRING_NUM")?;
            self.backend
                .set_vring_addr(
                    idx,
                    &VringConfigData {
                        queue_max_size: QUEUE_SIZE,
                        queue_size: queue.size,
                        flags: 0,
                        desc_table_addr: queue.desc,
                        used_ring_addr: queue.used,
                        avail_ring_addr: queue.avail,
                        log_addr: None,
                    },
                )
                .context("VHOST_SET_VRING_ADDR")?;
            self.backend
                .set_vring_base(idx, queue.last)
                .context("VHOST_SET_VRING_BASE")?;
            self.backend
                .set_vring_kick(idx, &self.kick_fds[idx])
                .context("VHOST_SET_VRING_KICK")?;

            let call_fd = self
                .msix
                .call_fd(queue.vec)
                .context("queue has no interrupt to signal")?;
            self.backend
                .set_vring_call(idx, call_fd)
                .context("VHOST_SET_VRING_CALL")?;
        }

        self.backend
            .set_guest_cid(self.cid)
            .context("VHOST_VSOCK_SET_GUEST_CID")?;
        self.backend.start().context("VHOST_VSOCK_SET_RUNNING")?;

        self.running = true;
        log::info!("vhost-vsock running, guest cid {}", self.cid);
        Ok(())
    }

    fn reset(&mut self) {
        if self.running {
            if let Err(err) = self.backend.stop() {
                log::warn!("failed to stop vhost-vsock: {err}");
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

pub struct VsockDevice {
    inner: Mutex<Inner>,
}

impl VsockDevice {
    /// Open the host's vhost-vsock and prepare a device for `cid`.
    pub fn new(cid: u64, mem: Arc<GuestMemoryMmap>) -> Result<Self> {
        anyhow::ensure!(cid > VMADDR_CID_HOST, "guest cid must be greater than 2");
        let backend = VhostVsockBackend::new(mem.clone())
            .context("failed to open /dev/vhost-vsock — is the vhost_vsock module loaded?")?;
        let kick_fds = (0..NUM_QUEUES)
            .map(|_| EventFd::new(0).context("failed to create vsock kick eventfd"))
            .collect::<Result<Vec<_>>>()?;

        let (cfg, msix_cap) = Self::build_pci_config();
        log::info!("virtio-vsock: guest cid {cid}");
        Ok(Self {
            inner: Mutex::new(Inner {
                cid,
                com: ComCfg::default(),
                qs: 0,
                queues: new_queues(),
                isr: 0,
                cfg_vec: VIRTQ_MSI_NO_VECTOR,
                mem: Some(mem),
                msix: MsixTable::default(),
                cfg,
                msix_cap,
                backend,
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
        // Class 0x078000: communication controller, other.
        let mut cfg = PciConfig::new(
            0x1AF4,
            PCI_DEVICE_ID,
            0x01,
            0x07_80_00,
            0x1AF4,
            VIRTIO_ID_VSOCK,
        );
        cfg.set_bar_mem(0, BAR0_SIZE);
        cfg.set_irq_pin(1);
        cfg.add_virtio_cap(1, 0, OFF_COMMON as u32, 0x38);
        cfg.add_virtio_notify_cap(0, OFF_NOTIFY as u32, 0x100, NOTIFY_MULT);
        cfg.add_virtio_cap(3, 0, OFF_ISR as u32, 1);
        cfg.add_virtio_cap(4, 0, OFF_DEVICE as u32, 8);
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
                        log::error!("failed to start vhost-vsock: {err:#}");
                    }
                }
            }
            CFG_QUEUE_SEL => i.qs = v2,
            CFG_QUEUE_SIZE => i.sqm().size = v2,
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
            // Device config: the 64-bit guest CID.
            let i = self.inner.lock().unwrap();
            let bytes = i.cid.to_le_bytes();
            let s = (o - OFF_DEVICE) as usize;
            let e = (s + d.len()).min(8);
            if s < 8 {
                d[..e - s].copy_from_slice(&bytes[s..e]);
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
            // The notify address identifies the queue; kick the kernel.
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

impl PciDevice for VsockDevice {
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
