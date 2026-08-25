//! Virtio net device over PCI transport, backed by the host's vhost-net.
//!
//! Same shape as the vsock device: we emulate the transport — config space,
//! feature negotiation, queue addresses — and hand the queues to the kernel,
//! which moves packets between the guest and a tap interface without this
//! process being involved.
//!
//! The device owns its tap, because the tap's offload settings can only be
//! chosen once the guest has said which offloads it understands.

use crate::common::*;
use crate::tap::{TUN_F_CSUM, TUN_F_TSO_ECN, TUN_F_TSO4, TUN_F_TSO6, TUN_F_UFO, Tap};
use anyhow::{Context, Result};
use pci::config::{PCIE_TYPE_RC_INTEGRATED, PciConfig};
use pci::{MsiRouter, MsiVector, PciDevice};
use std::sync::{Arc, Mutex};
use vhost::net::VhostNet;
use vhost::vhost_kern::net::Net as VhostNetBackend;
use vhost::{VhostBackend, VringConfigData};
use vm_memory::{Address, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};
use vmm_sys_util::eventfd::EventFd;

const QUEUE_SIZE: u16 = 256;
/// rx and tx. There is no control queue: vhost-net does not service one, and
/// without it the guest cannot ask for anything we would have to answer.
const NUM_QUEUES: usize = 2;
/// One per queue plus a config vector.
const MSIX_VECTORS: u16 = 4;

/// virtio device type 1; PCI device id is 0x1040 + type.
const VIRTIO_ID_NET: u16 = 1;
const PCI_DEVICE_ID: u16 = 0x1040 + VIRTIO_ID_NET;

// virtio-net feature bits.
const VIRTIO_NET_F_CSUM: u64 = 1 << 0;
const VIRTIO_NET_F_GUEST_CSUM: u64 = 1 << 1;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_GUEST_TSO4: u64 = 1 << 7;
const VIRTIO_NET_F_GUEST_TSO6: u64 = 1 << 8;
const VIRTIO_NET_F_GUEST_ECN: u64 = 1 << 9;
const VIRTIO_NET_F_GUEST_UFO: u64 = 1 << 10;
const VIRTIO_NET_F_HOST_TSO4: u64 = 1 << 11;
const VIRTIO_NET_F_HOST_TSO6: u64 = 1 << 12;
const VIRTIO_NET_F_HOST_ECN: u64 = 1 << 13;
const VIRTIO_NET_F_HOST_UFO: u64 = 1 << 14;
const VIRTIO_NET_F_MRG_RXBUF: u64 = 1 << 15;

/// Size of `virtio_net_hdr_v1`, which carries a merged-buffer count.
const VNET_HDR_SIZE_MRG: i32 = 12;
/// Size of the older `virtio_net_hdr`, without it.
const VNET_HDR_SIZE_PLAIN: i32 = 10;

/// Device config space: just the MAC address.
const CONFIG_SIZE: u32 = 6;

/// How to set up the guest's network link.
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// Tap interface to open. Exact -- the host created it, so nesbox is not
    /// choosing the name.
    pub tap_name: String,
    /// Guest MAC. Generated if absent.
    pub mac: Option<[u8; 6]>,
}

/// A locally-administered unicast MAC, so it cannot collide with real hardware.
fn default_mac() -> [u8; 6] {
    // Bit 1 of the first octet marks it locally administered, bit 0 clear
    // keeps it unicast.
    [0x02, 0x00, 0x00, 0x00, 0x00, 0x01]
}

struct Inner {
    mac: [u8; 6],
    com: ComCfg,
    qs: u16,
    queues: [QState; NUM_QUEUES],
    isr: u8,
    cfg_vec: u16,
    mem: Option<Arc<GuestMemoryMmap>>,
    msix: MsixTable<4>,
    cfg: [u8; 256],
    msix_cap: u16,
    backend: VhostNetBackend<Arc<GuestMemoryMmap>>,
    tap: Tap,
    /// Features the kernel backend admits to supporting. Anything outside this
    /// set makes `VHOST_SET_FEATURES` fail with EOPNOTSUPP, so the guest's
    /// acked features are masked with it before being passed down.
    backend_features: u64,
    kick_fds: Vec<EventFd>,
    running: bool,
}

impl Inner {
    fn features(&self) -> u64 {
        // Offloads are worth advertising: they are most of the reason for
        // choosing an in-kernel backend over a userspace one.
        VIRTIO_F_VERSION_1
            | VIRTIO_NET_F_MAC
            | VIRTIO_NET_F_MRG_RXBUF
            | VIRTIO_NET_F_CSUM
            | VIRTIO_NET_F_GUEST_CSUM
            | VIRTIO_NET_F_GUEST_TSO4
            | VIRTIO_NET_F_GUEST_TSO6
            | VIRTIO_NET_F_GUEST_ECN
            | VIRTIO_NET_F_GUEST_UFO
            | VIRTIO_NET_F_HOST_TSO4
            | VIRTIO_NET_F_HOST_TSO6
            | VIRTIO_NET_F_HOST_ECN
            | VIRTIO_NET_F_HOST_UFO
    }

    /// Which tap offloads follow from what the guest accepted.
    ///
    /// The GUEST_* bits are the guest promising it can receive frames the
    /// kernel has not segmented, so those are exactly the ones that let the
    /// tap pass large frames straight through.
    fn tap_offload_flags(acked: u64) -> u32 {
        let mut flags = 0;
        if acked & VIRTIO_NET_F_GUEST_CSUM != 0 {
            flags |= TUN_F_CSUM;
        }
        // Every segmentation offload rides on checksum offload; asking for one
        // without the other is rejected.
        if flags & TUN_F_CSUM != 0 {
            if acked & VIRTIO_NET_F_GUEST_TSO4 != 0 {
                flags |= TUN_F_TSO4;
            }
            if acked & VIRTIO_NET_F_GUEST_TSO6 != 0 {
                flags |= TUN_F_TSO6;
            }
            if acked & VIRTIO_NET_F_GUEST_ECN != 0 {
                flags |= TUN_F_TSO_ECN;
            }
            if acked & VIRTIO_NET_F_GUEST_UFO != 0 {
                flags |= TUN_F_UFO;
            }
        }
        flags
    }

    /// Hand the queues and the tap to the kernel and start moving packets.
    fn activate(&mut self) -> Result<()> {
        if self.running {
            return Ok(());
        }
        let mem = self.mem.clone().context("guest memory not attached")?;
        let acked = self.com.df & self.features();

        // The header size has to agree with the guest's view before any frame
        // crosses the tap, or every one of them is misparsed.
        let hdr_size = if acked & VIRTIO_NET_F_MRG_RXBUF != 0 {
            VNET_HDR_SIZE_MRG
        } else {
            VNET_HDR_SIZE_PLAIN
        };
        let offloads = Self::tap_offload_flags(acked);
        // Worth having on hand: if traffic does not flow, the first question is
        // always whether the guest and the tap agreed on the header size, and
        // the second is which offloads are actually in play.
        log::debug!(
            "virtio-net negotiated features {acked:#x}, vnet header {hdr_size} bytes, \
             tap offloads {offloads:#04x}"
        );
        self.tap.set_vnet_hdr_size(hdr_size)?;
        self.tap.set_offload(offloads)?;

        self.backend.set_owner().context("VHOST_SET_OWNER")?;
        self.backend
            .set_features(acked & self.backend_features)
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

        for (idx, queue) in self.queues.iter().enumerate() {
            // As with vhost-vsock, the kernel backend takes guest physical
            // addresses and translates them itself.
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

        // Attaching the tap is what actually starts the flow, so it goes last:
        // by this point every queue is ready to be serviced.
        for idx in 0..NUM_QUEUES {
            self.backend
                .set_backend(idx, Some(self.tap.file()))
                .context("VHOST_NET_SET_BACKEND")?;
        }

        self.running = true;
        log::info!(
            "vhost-net running on tap {}, guest mac {}",
            self.tap.name(),
            format_mac(&self.mac)
        );
        Ok(())
    }

    fn reset(&mut self) {
        if self.running {
            for idx in 0..NUM_QUEUES {
                if let Err(err) = self.backend.set_backend(idx, None) {
                    log::warn!("failed to detach vhost-net queue {idx}: {err}");
                }
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

fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn new_queues() -> [QState; NUM_QUEUES] {
    std::array::from_fn(|i| QState {
        size: QUEUE_SIZE,
        vec: i as u16,
        ..Default::default()
    })
}

pub struct NetDevice {
    inner: Mutex<Inner>,
}

impl NetDevice {
    /// Create the tap, open vhost-net, and prepare the device. Nothing flows
    /// until the guest driver reaches DRIVER_OK.
    pub fn new(config: &NetConfig, mem: Arc<GuestMemoryMmap>) -> Result<Self> {
        // Addressing and bridging belong to whoever set the host up; by the
        // time we get here the tap is already wherever it should be.
        let tap = Tap::open(&config.tap_name)?;

        let backend = VhostNetBackend::new(mem.clone())
            .context("failed to open /dev/vhost-net — is the vhost_net module loaded?")?;
        let backend_features = backend.get_features().context("VHOST_GET_FEATURES")?;

        let kick_fds = (0..NUM_QUEUES)
            .map(|_| EventFd::new(0).context("failed to create net kick eventfd"))
            .collect::<Result<Vec<_>>>()?;

        let mac = config.mac.unwrap_or_else(default_mac);
        let (cfg, msix_cap) = Self::build_pci_config();
        Ok(Self {
            inner: Mutex::new(Inner {
                mac,
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
                tap,
                backend_features,
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
        // Class 0x020000: network controller, ethernet.
        let mut cfg = PciConfig::new(
            0x1AF4,
            PCI_DEVICE_ID,
            0x01,
            0x02_00_00,
            0x1AF4,
            VIRTIO_ID_NET,
        );
        cfg.set_bar_mem(0, BAR0_SIZE);
        cfg.set_irq_pin(1);
        cfg.add_virtio_cap(1, 0, OFF_COMMON as u32, 0x38);
        cfg.add_virtio_notify_cap(0, OFF_NOTIFY as u32, 0x100, NOTIFY_MULT);
        cfg.add_virtio_cap(3, 0, OFF_ISR as u32, 1);
        cfg.add_virtio_cap(4, 0, OFF_DEVICE as u32, CONFIG_SIZE);
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
                        log::error!("failed to start vhost-net: {err:#}");
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
            // Device config: the MAC address.
            let i = self.inner.lock().unwrap();
            let s = (o - OFF_DEVICE) as usize;
            let e = (s + d.len()).min(i.mac.len());
            if s < i.mac.len() {
                d[..e - s].copy_from_slice(&i.mac[s..e]);
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

impl PciDevice for NetDevice {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segmentation_offload_requires_checksum_offload() {
        // The kernel rejects TSO without CSUM, so a guest that took the
        // segmentation features but not checksums must get neither.
        let acked = VIRTIO_NET_F_GUEST_TSO4 | VIRTIO_NET_F_GUEST_TSO6;
        assert_eq!(Inner::tap_offload_flags(acked), 0);
    }

    #[test]
    fn offloads_follow_the_guest_features() {
        let acked = VIRTIO_NET_F_GUEST_CSUM
            | VIRTIO_NET_F_GUEST_TSO4
            | VIRTIO_NET_F_GUEST_TSO6
            | VIRTIO_NET_F_GUEST_ECN
            | VIRTIO_NET_F_GUEST_UFO;
        assert_eq!(
            Inner::tap_offload_flags(acked),
            TUN_F_CSUM | TUN_F_TSO4 | TUN_F_TSO6 | TUN_F_TSO_ECN | TUN_F_UFO
        );
    }

    #[test]
    fn a_guest_taking_nothing_gets_no_offloads() {
        assert_eq!(Inner::tap_offload_flags(VIRTIO_F_VERSION_1), 0);
    }

    #[test]
    fn the_default_mac_is_locally_administered_and_unicast() {
        let mac = default_mac();
        assert_eq!(mac[0] & 0x02, 0x02, "must be locally administered");
        assert_eq!(mac[0] & 0x01, 0x00, "must not be a multicast address");
    }
}
