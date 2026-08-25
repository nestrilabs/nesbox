use anyhow::Context;
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
    /// Optional network device. Absent means the guest has no link at all.
    #[serde(default)]
    pub network: Option<Network>,
    /// Optional GPU. Absent means the guest has no display device.
    #[serde(default)]
    pub gpu: Option<Gpu>,
    /// Unix socket to serve a JSON metrics snapshot on. Absent means no surface,
    /// which is right for a hand-driven box and wrong for a supervised one.
    #[serde(default, rename = "stats-socket")]
    pub stats_socket: Option<PathBuf>,
    /// seccomp-bpf confinement: `enforce`, `audit`, or `off`.
    ///
    /// `enforce` kills the process on a syscall outside the policy. `audit`
    /// reports which syscall it was and exits, which is how the policy is
    /// extended. Default `enforce` -- a security control that is off by default
    /// is not a control.
    #[serde(default = "default_seccomp")]
    pub seccomp: String,
}

fn default_seccomp() -> String {
    "enforce".to_string()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
// `deny_unknown_fields` because `vram-limit-mib` is a safety limit: misspell it
// and serde would silently leave the guest unbounded. A config error must be an
// error, not a default.
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Gpu {
    /// Host render node to give the guest, e.g. `/dev/dri/renderD128`.
    #[serde(default = "default_render_node")]
    pub render_node: PathBuf,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    /// Device memory this guest may hold, in MiB.
    ///
    /// Omitted, the guest may allocate until the card is exhausted -- fine when
    /// it is the only guest, unsafe when it is not, since VRAM cannot be
    /// reclaimed from a guest that has taken it.
    #[serde(default)]
    pub vram_limit_mib: Option<u64>,
    /// Bytes the guest may map into the host-visible window (BAR2), in MiB.
    ///
    /// Every mapping costs host address space and a KVM memory slot, and nothing
    /// in the protocol makes a guest ask for a sensible number of them. Omitted
    /// means unbounded.
    #[serde(default)]
    pub host_visible_window_mib: Option<u64>,
    /// Live window mappings allowed. Omitted means unbounded.
    ///
    /// Each mapping is a KVM memory slot and KVM has a few thousand, so a guest
    /// mapping single pages could exhaust them. That already fails safely, so this
    /// is unbounded by default: no measurement yet says what a real workload
    /// needs, and a cap guessed too low breaks it.
    #[serde(default)]
    pub host_visible_max_mappings: Option<u32>,
}

fn default_render_node() -> PathBuf {
    PathBuf::from("/dev/dri/renderD128")
}
fn default_width() -> u32 {
    1920
}
fn default_height() -> u32 {
    1080
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Network {
    /// Tap interface to create. `%d` is filled in by the kernel with the
    /// lowest free number, which is what you want when several VMs run at
    /// once.
    /// Tap to open. Exact: the host created it, so nesbox is not choosing.
    pub tap_name: String,
    /// Guest MAC. Generated if absent; a supervising agent would normally
    /// supply one so the address is stable across restarts.
    pub mac: Option<String>,
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
    /// Host CPUs the vCPU threads may run on. Empty or absent means no
    /// affinity is set and the host scheduler places them freely.
    ///
    /// One set shared by every vCPU thread, rather than a pin per vCPU. On a
    /// chiplet CPU the win is keeping a guest inside one L3 domain, and a set
    /// gets that while still letting the host balance within it. Pinning
    /// one-to-one would also mean deciding which vCPUs land on SMT siblings,
    /// and the guest cannot currently be told which of its CPUs are siblings
    /// -- nesbox's ACPI tables do not describe CPU topology -- so it would
    /// schedule against a layout it cannot see.
    ///
    /// Applied verbatim. Which CPUs belong to a guest is the caller's decision;
    /// this end only carries it out, the same way `vcpu_count` works.
    #[serde(default)]
    pub cpu_affinity: Vec<usize>,
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
            cpu_affinity: Vec::new(),
        }
    }
}

#[cfg(test)]
mod machine_config_tests {
    use super::*;

    /// Every config written before `cpu_affinity` existed must still parse, and
    /// must mean "no affinity" rather than "no CPUs" — a hand-written config
    /// used to debug a box is the common case. A supervising agent should always
    /// emit the field, empty where the host's cache topology could not be read.
    #[test]
    fn a_config_without_cpu_affinity_still_parses() {
        let json = r#"{ "vcpu_count": 4, "mem_size_mib": 8192 }"#;
        let mc: MachineConfig = serde_json::from_str(json).expect("parses");
        assert_eq!(mc.vcpu_count, 4);
        assert!(
            mc.cpu_affinity.is_empty(),
            "an absent field must not imply a CPU set"
        );
    }

    #[test]
    fn a_cpu_set_round_trips() {
        let json = r#"{ "vcpu_count": 14, "mem_size_mib": 28672,
                        "cpu_affinity": [1,2,3,4,5,6,7,17,18,19,20,21,22,23] }"#;
        let mc: MachineConfig = serde_json::from_str(json).expect("parses");
        assert_eq!(mc.cpu_affinity.len(), 14);
        assert_eq!(mc.cpu_affinity.first(), Some(&1));
        assert_eq!(mc.cpu_affinity.last(), Some(&23));

        let back: MachineConfig =
            serde_json::from_str(&serde_json::to_string(&mc).expect("serialises")).expect("parses");
        assert_eq!(back.cpu_affinity, mc.cpu_affinity);
    }
}

#[cfg(test)]
mod whole_config_tests {
    use super::*;

    /// A complete config of the shape used to test a box by hand.
    ///
    /// Here because the fields are spread across several structs with mixed
    /// casing, and "does my config still parse" is otherwise only answerable
    /// by starting a VM.
    const HAND_WRITTEN: &str = r#"{
      "boot-source": {
        "kernel_image_path": "/mnt/INSTANCES/DEV/vmlinux",
        "boot_args": "console=hvc0 root=/dev/vda ro nestri.ip=192.168.128.11/24 nestri.gw=192.168.128.1"
      },
      "drives": [
        { "drive_id": "rootfs", "path_on_host": "/mnt/INSTANCES/DEV/testrootfs.ext4",
          "is_root_device": true, "is_read_only": true }
      ],
      "machine-config": {
        "vcpu_count": 8, "mem_size_mib": 8192, "cpu_affinity": [0,1,2,3]
      },
      "gpu": { "render-node": "/dev/dri/renderD128", "width": 1920, "height": 1080 },
      "network": { "tap-name": "nesbox0", "mac": "02:00:00:00:00:01" },
      "shared-directories": [
        { "tag": "install", "path-on-host": "/mnt/GAMEDRIVE/nes/632360", "read-only": true },
        { "tag": "user", "path-on-host": "/mnt/INSTANCES/users/usr_x", "read-only": false }
      ]
    }"#;

    #[test]
    fn a_hand_written_config_parses() {
        let config: VmConfig = serde_json::from_str(HAND_WRITTEN).expect("parses");
        let net = config.network.as_ref().expect("has a network");
        assert_eq!(net.tap_name, "nesbox0");
        assert_eq!(config.machine_config.cpu_affinity, vec![0, 1, 2, 3]);
        assert_eq!(config.machine_config.vcpu_count, 8);
    }

    /// The network section is down to what nesbox can act on by itself.
    /// Addresses, netmasks and bridges were host administration wearing a VM
    /// config's clothes, and they are the setup script's now.
    #[test]
    fn the_network_section_is_only_a_tap_and_a_mac() {
        let net: Network =
            serde_json::from_str(r#"{ "tap-name": "nesbox1" }"#).expect("parses");
        assert_eq!(net.tap_name, "nesbox1");
        assert!(net.mac.is_none());
    }

    /// An exact name, because the host made the device. `%d` only ever worked
    /// when nesbox was the one creating it.
    #[test]
    fn a_tap_name_is_required() {
        assert!(serde_json::from_str::<Network>(r#"{ "mac": "02:00:00:00:00:01" }"#).is_err());
    }
}
