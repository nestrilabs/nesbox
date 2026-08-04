use serde::{Deserialize, Serialize};
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
