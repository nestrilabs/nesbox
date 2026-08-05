use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct VmConfig {
    #[serde(default)]
    pub boot_source: BootSource,
    #[serde(default)]
    pub drives: Vec<Drive>,
    #[serde(default)]
    pub machine_config: MachineConfig,
    /// Optional vsock device; the control channel a launcher talks over.
    #[serde(default)]
    pub vsock: Option<Vsock>,
    /// Host directories exported to the guest over virtio-fs.
    #[serde(default)]
    pub shared_directories: Vec<SharedDirectory>,
    /// Optional network device. Absent means the guest has no link at all.
    #[serde(default)]
    pub network: Option<Network>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Network {
    /// Tap interface to create. `%d` is filled in by the kernel with the
    /// lowest free number, which is what you want when several VMs run at
    /// once.
    #[serde(default = "default_tap_name")]
    pub tap_name: String,
    /// Address for the host end of the tap. The guest is expected to take
    /// another address on the same subnet and route through this one.
    #[serde(default = "default_host_ip")]
    pub host_ip: Ipv4Addr,
    #[serde(default = "default_netmask")]
    pub netmask: Ipv4Addr,
    /// Guest MAC, as `aa:bb:cc:dd:ee:ff`. Generated if absent.
    #[serde(default)]
    pub mac: Option<String>,
}

fn default_tap_name() -> String {
    "nesbox%d".to_string()
}
fn default_host_ip() -> Ipv4Addr {
    Ipv4Addr::new(172, 30, 0, 1)
}
fn default_netmask() -> Ipv4Addr {
    Ipv4Addr::new(255, 255, 255, 0)
}

impl Network {
    /// Parse the configured MAC, if there is one.
    pub fn parsed_mac(&self) -> anyhow::Result<Option<[u8; 6]>> {
        let Some(text) = &self.mac else {
            return Ok(None);
        };
        let octets: Vec<&str> = text.split(':').collect();
        anyhow::ensure!(
            octets.len() == 6,
            "mac {text:?} should have six colon-separated octets"
        );
        let mut mac = [0u8; 6];
        for (slot, octet) in mac.iter_mut().zip(octets) {
            *slot = u8::from_str_radix(octet, 16)
                .with_context(|| format!("mac {text:?} has a bad octet {octet:?}"))?;
        }
        anyhow::ensure!(
            mac[0] & 0x01 == 0,
            "mac {text:?} is a multicast address; the low bit of the first octet must be clear"
        );
        Ok(Some(mac))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct SharedDirectory {
    /// Mount tag the guest uses: `mount -t virtiofs <tag> /somewhere`.
    pub tag: String,
    pub path_on_host: PathBuf,
    /// Export read-only. A game's install directory wants this: it makes
    /// "only the downloader writes here" true by construction.
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Vsock {
    /// Context ID the guest is reachable at. Must be greater than 2.
    pub guest_cid: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BootSource {
    pub kernel_image_path: PathBuf,
    #[serde(default)]
    pub boot_args: String,
}

impl Default for BootSource {
    fn default() -> Self {
        Self {
            kernel_image_path: PathBuf::from("vmlinux"),
            boot_args: "console=hvc0 root=/dev/vda rw".to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: PathBuf,
    pub is_root_device: bool,
    #[serde(default)]
    pub is_read_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MachineConfig {
    #[serde(default = "default_vcpus")]
    pub vcpu_count: u8,
    #[serde(default = "default_mem_size")]
    pub mem_size_mib: usize,
}

fn default_vcpus() -> u8 {
    2
}
fn default_mem_size() -> usize {
    2048
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            vcpu_count: default_vcpus(),
            mem_size_mib: default_mem_size(),
        }
    }
}
