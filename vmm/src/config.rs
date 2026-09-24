use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A box's configuration, as the caller wrote it.
///
/// `deny_unknown_fields` on purpose. A key this build does not understand is
/// almost always one of two things, and both are worse silently: a typo, or a
/// config written for a build that has a feature this one does not. The second
/// is not hypothetical -- a config asking for `gpu-forward` was handed to a
/// nesbox built before that device existed, and it booted a guest with no GPU
/// and said nothing. The guest came up, the forwarding backend sat waiting on
/// a socket nobody connected to, and the failure surfaced as a driver inside
/// the guest finding no hardware.
///
/// Refusing the config names the key instead, at the point the mistake was
/// made. The nested sections that already did this (`Gpu`, `GpuForward`) were
/// right; the top level was the one that most needed it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
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
    /// Optional GPU ioctl forwarding device.
    ///
    /// Independent of `gpu`: that one gives the guest a rendering device this
    /// process drives, while this one carries the guest's own driver ioctls to
    /// a separate backend. A guest may have either, or in principle both,
    /// since they are different devices serving different guest drivers.
    #[serde(default, rename = "gpu-forward")]
    pub gpu_forward: Option<GpuForward>,
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
    /// Enter a private user and network namespace before the guest runs.
    ///
    /// seccomp allows `socket` and `connect`, so a compromised device model
    /// otherwise has the host's network. This takes the network away entirely --
    /// the guest keeps its own link, because the tap is opened before the
    /// unshare and an open descriptor is unaffected by it.
    ///
    /// Default off, and not because it is unimportant. Entering a user namespace
    /// maps only this uid and gid, so access that depends on supplementary group
    /// membership stops working, and Mesa opens the DRM render node *after* this
    /// point. Where the render node is `0660 root:render` this will stop the GPU
    /// working; where it is `0666` it is free. See
    /// `isolation::enter_network_namespace` for the full argument, and turn it
    /// on deliberately after testing on the host it will run on.
    #[serde(default)]
    pub unshare_network: bool,
}

fn default_seccomp() -> String {
    "enforce".to_string()
}

/// GPU ioctl forwarding over a vhost-user backend.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct GpuForward {
    /// Unix socket a forwarding backend is already listening on.
    ///
    /// The backend is started separately and holds the real host device
    /// descriptors. This process only carries the transport, so it needs no
    /// access to the GPU itself.
    pub socket: PathBuf,
    /// Where the host GPU driver publishes itself.
    ///
    /// Overridable so the device can be exercised against a fixture tree
    /// rather than a live driver.
    #[serde(default = "default_proc_nvidia")]
    pub proc_nvidia: PathBuf,
}

fn default_proc_nvidia() -> PathBuf {
    PathBuf::from("/proc/driver/nvidia")
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
    /// Microseconds the GPU worker looks at the control queue before sleeping.
    ///
    /// **Defaulted on, unlike a drive's.** A guest kicks its disk when it has
    /// I/O; it kicks the GPU once per command submission, measured at ~24,000 a
    /// second under one game -- a gap of about 42 us, which is less than a
    /// thread wakeup and a scheduler round trip. And the guest waits on each
    /// submission's fence before making the next, so the GPU is idle for every
    /// microsecond of that wakeup rather than working on something else.
    ///
    /// The cost is a core per *active* guest: the spin is bounded to this many
    /// microseconds after each wake, so an idle box spins once and sleeps, but
    /// a box under load spends most of the window spinning. On a host packed
    /// with guests that is the wrong trade -- set it to `0` there.
    #[serde(default = "default_gpu_poll_us")]
    pub poll_us: u64,
}

/// Long enough to cover the gap between submissions from a guest running a
/// game, which is what makes the spin land on work rather than on a deadline.
///
/// # Why 50 and not more
///
/// It was 200 for a while, on a measurement taken when the guest was making
/// 30,000 forwarded calls a second and the mean gap between them was 33 us:
/// the worker was reaching the deadline and sleeping on a long tail, and
/// widening the window bought 3.6% more throughput for 13 points of a core.
///
/// That guest no longer exists. Caching the memory query in Mesa and placing
/// blob resources inside the window instead of giving each one a memory slot
/// took the call rate to 4,100 a second, and the worker now sleeps about half
/// the time because there is genuinely nothing to do. A window four times the
/// mean gap is spending a core to catch a tail that is mostly gone.
///
/// So: back to covering the ordinary gap, and the core goes back to the box.
/// Raise it on a host that is running one guest and wants the last few percent.
const fn default_gpu_poll_us() -> u64 {
    50
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
    /// Bypass the host page cache.
    ///
    /// Absent -- the default -- means direct I/O where the backing file
    /// supports it and buffered where it does not, which is what a box running
    /// several guests wants: without it every guest byte is cached twice, once
    /// by the host and once by the guest, and an `io.max` bound on the VM stops
    /// applying as soon as the host has the image cached.
    ///
    /// `true` refuses to start if the file cannot do direct I/O, for a box
    /// whose isolation depends on it. `false` keeps the host page cache, which
    /// is faster for a single guest on a machine with RAM to spare.
    #[serde(default)]
    pub direct: Option<bool>,
    /// Virtqueues for this drive. Absent means one per vCPU, up to the four a
    /// guest's block layer can keep busy.
    ///
    /// One queue means every guest CPU contends on one ring and one worker.
    #[serde(default)]
    pub num_queues: Option<u16>,
    /// Microseconds a queue's worker looks at its ring before sleeping.
    ///
    /// Absent or zero sleeps at once, which costs a thread wakeup on every
    /// request a guest submits singly -- at queue depth one that wakeup is a
    /// large part of the latency the guest sees. A non-zero value spends a core
    /// to avoid it. Worth it for a drive under small random I/O on a host with
    /// cores to spare, and not worth it for a box packed with idle guests.
    #[serde(default)]
    pub poll_us: Option<u64>,
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
    /// One set shared by every vCPU thread, which keeps a guest inside one L3
    /// domain while still letting the host balance within it. `vcpu_pins`
    /// replaces it for the vCPUs when set.
    ///
    /// Applied verbatim. Which CPUs belong to a guest is the caller's decision;
    /// this end only carries it out, the same way `vcpu_count` works.
    #[serde(default)]
    pub cpu_affinity: Vec<usize>,
    /// One host CPU per vCPU, in vCPU order. Empty means no pins, and the
    /// vCPUs share `cpu_affinity` instead.
    ///
    /// A pin is only better than a set when nothing else runs on the pinned
    /// CPU: a pinned vCPU cannot escape to an idle core when a host task lands
    /// on its own. So this is for CPUs the host has set aside for the guest, and
    /// is what `dedicated` requires.
    #[serde(default)]
    pub vcpu_pins: Vec<usize>,
    /// SMT threads per guest core: 1 or 2. With 2, vCPUs `2k` and `2k+1` are
    /// told they are siblings, and must be pinned to two threads of one host
    /// core for that to be true.
    ///
    /// The guest's scheduler spreads work across cores before doubling up on
    /// siblings, but only if it knows which CPUs are siblings. Told nothing, it
    /// treats two threads of one core as two cores and happily puts two busy
    /// threads on one of them. Requires `vcpu_pins`, since otherwise nothing
    /// keeps the host's siblings where the guest was told they are.
    #[serde(default = "default_threads_per_core")]
    pub threads_per_core: u8,
    /// Host CPUs for every thread that is not a vCPU: device workers, the
    /// stats server, virtiofsd and the kernel's vhost workers. Empty means the
    /// same set as `cpu_affinity`.
    ///
    /// These threads serve the guest, but they are not the guest. Kept on the
    /// guest's own CPUs they compete with its vCPUs, and under pins they would
    /// land on one vCPU's core and halve it.
    #[serde(default)]
    pub io_affinity: Vec<usize>,
    /// The pinned CPUs belong to this guest alone, and nothing else on the host
    /// will run on them.
    ///
    /// This is a promise the caller makes; nesbox cannot check it. On the
    /// strength of it, a halting vCPU halts the physical core instead of
    /// exiting to the host, spin-waits stop exiting, and the guest is told its
    /// vCPUs are never preempted, which moves it off paravirtual spinlocks. All
    /// of that is right only when the promise holds. With another task sharing
    /// a pinned CPU, a guest spinning on a lock whose holder is preempted spins
    /// until the host scheduler lets the holder run again.
    #[serde(default)]
    pub dedicated: bool,
    /// An inherited descriptor, open for writing on the `cgroup.threads` of
    /// the cgroup that owns the pinned CPUs. Each vCPU thread moves itself
    /// there through it before pinning.
    ///
    /// For a host that sets CPUs aside at runtime with an isolated cpuset
    /// partition rather than at boot: an isolated CPU is not in this process's
    /// cpuset, so a pin onto it is refused until the thread is inside the
    /// partition. A descriptor rather than a path because the kernel checks a
    /// cgroup move against whoever opened the file, so whoever set the
    /// partition up can open it and hand it on, and nesbox needs neither the
    /// privilege nor a view of the cgroup filesystem to use it.
    #[serde(default)]
    pub vcpu_cgroup_fd: Option<i32>,
    /// The same for the cgroup this process started in, which holds the I/O
    /// CPUs. Kernel workers that a vCPU thread creates are born in the vCPU
    /// cgroup, on the pinned CPUs, and are moved back out through this.
    #[serde(default)]
    pub io_cgroup_fd: Option<i32>,
    /// What page size backs guest RAM. `transparent` if absent.
    #[serde(default)]
    pub hugepages: HugePages,
    /// Fault in every page of guest RAM on a background thread once the VM is
    /// built, instead of on the guest's first touch.
    ///
    /// A first touch allocates and zeroes a page with the vCPU stopped, and for
    /// a huge page that is a stall long enough to land in a frame. Prefaulted,
    /// that cost is paid while the guest boots. The price is density: the
    /// host commits all of guest RAM up front, where untouched memory would
    /// otherwise cost nothing.
    #[serde(default)]
    pub prefault: bool,
}

/// The page size behind guest RAM.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum HugePages {
    /// Transparent huge pages, asked for and taken where the kernel has them.
    /// Nothing needs reserving, and nothing is guaranteed: once host memory is
    /// fragmented the guest quietly runs on 4 KiB pages.
    #[default]
    #[serde(rename = "transparent")]
    Transparent,
    /// 2 MiB pages from the host's hugetlb pool.
    #[serde(rename = "2m")]
    Huge2M,
    /// 1 GiB pages from the host's hugetlb pool.
    #[serde(rename = "1g")]
    Huge1G,
}

impl HugePages {
    /// The hugetlb page size in bytes, or `None` for transparent pages.
    pub fn hugetlb_size(self) -> Option<u64> {
        match self {
            HugePages::Transparent => None,
            HugePages::Huge2M => Some(2 << 20),
            HugePages::Huge1G => Some(1 << 30),
        }
    }
}

fn default_vcpus() -> u8 {
    2
}
fn default_mem_size() -> usize {
    2048
}
fn default_threads_per_core() -> u8 {
    1
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            vcpu_count: default_vcpus(),
            mem_size_mib: default_mem_size(),
            cpu_affinity: Vec::new(),
            vcpu_pins: Vec::new(),
            threads_per_core: default_threads_per_core(),
            io_affinity: Vec::new(),
            dedicated: false,
            vcpu_cgroup_fd: None,
            io_cgroup_fd: None,
            hugepages: HugePages::default(),
            prefault: false,
        }
    }
}

impl MachineConfig {
    /// Refuse a placement that cannot be carried out as written.
    ///
    /// Each of these would otherwise run, and run wrong without saying so: a
    /// guest told it has siblings it does not have, a vCPU left unpinned in a
    /// box that promised the guest it was dedicated.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.vcpu_pins.is_empty() {
            anyhow::ensure!(
                self.vcpu_pins.len() == usize::from(self.vcpu_count),
                "vcpu_pins names {} CPUs for {} vCPUs",
                self.vcpu_pins.len(),
                self.vcpu_count
            );
            let mut sorted = self.vcpu_pins.clone();
            sorted.sort_unstable();
            sorted.dedup();
            anyhow::ensure!(
                sorted.len() == self.vcpu_pins.len(),
                "vcpu_pins names one CPU twice, so two vCPUs would share it"
            );
        }
        anyhow::ensure!(
            matches!(self.threads_per_core, 1 | 2),
            "threads_per_core must be 1 or 2, not {}",
            self.threads_per_core
        );
        anyhow::ensure!(
            self.threads_per_core == 1 || !self.vcpu_pins.is_empty(),
            "threads_per_core = 2 needs vcpu_pins: without them nothing keeps two \
             vCPUs on one host core"
        );
        anyhow::ensure!(
            !self.dedicated || !self.vcpu_pins.is_empty(),
            "dedicated needs vcpu_pins: a CPU cannot be dedicated to a vCPU that is \
             not pinned to it"
        );
        for (name, fd) in [
            ("vcpu_cgroup_fd", self.vcpu_cgroup_fd),
            ("io_cgroup_fd", self.io_cgroup_fd),
        ] {
            if let Some(fd) = fd {
                anyhow::ensure!(fd > 2, "{name} {fd} is one of stdin, stdout and stderr");
            }
        }
        anyhow::ensure!(
            self.vcpu_cgroup_fd.is_some() == self.io_cgroup_fd.is_some(),
            "vcpu_cgroup_fd and io_cgroup_fd come together: a thread that can \
             step into the partition but not back out leaves kernel workers \
             pinned inside it"
        );
        if let Some(page) = self.hugepages.hugetlb_size() {
            let page_mib = page >> 20;
            anyhow::ensure!(
                self.mem_size_mib as u64 % page_mib == 0,
                "mem_size_mib {} is not a whole number of {page_mib} MiB huge pages",
                self.mem_size_mib
            );
        }
        Ok(())
    }

    /// Where the threads that are not vCPUs go.
    pub fn io_cpus(&self) -> &[usize] {
        if self.io_affinity.is_empty() {
            &self.cpu_affinity
        } else {
            &self.io_affinity
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

    /// A config from before placement grew pins means exactly what it meant
    /// then: a shared set, one thread per core, nothing dedicated.
    #[test]
    fn an_old_config_keeps_its_old_meaning() {
        let json = r#"{ "vcpu_count": 4, "mem_size_mib": 8192, "cpu_affinity": [0,1,2,3] }"#;
        let mc: MachineConfig = serde_json::from_str(json).expect("parses");
        assert!(mc.vcpu_pins.is_empty());
        assert_eq!(mc.threads_per_core, 1);
        assert!(!mc.dedicated);
        assert_eq!(mc.io_cpus(), &[0, 1, 2, 3], "io threads follow the set");
        mc.validate().expect("an old config is valid");
    }

    fn pinned(pins: &[usize]) -> MachineConfig {
        MachineConfig {
            vcpu_count: pins.len() as u8,
            vcpu_pins: pins.to_vec(),
            ..MachineConfig::default()
        }
    }

    #[test]
    fn pins_must_cover_every_vcpu_exactly_once() {
        let mut short = pinned(&[2, 3]);
        short.vcpu_count = 4;
        assert!(short.validate().is_err());
        assert!(pinned(&[2, 2]).validate().is_err());
        pinned(&[2, 3, 10, 11])
            .validate()
            .expect("distinct and complete");
    }

    #[test]
    fn siblings_and_dedication_need_pins() {
        let smt = MachineConfig {
            threads_per_core: 2,
            ..MachineConfig::default()
        };
        assert!(smt.validate().is_err());
        let dedicated = MachineConfig {
            dedicated: true,
            ..MachineConfig::default()
        };
        assert!(dedicated.validate().is_err());

        let both = MachineConfig {
            threads_per_core: 2,
            dedicated: true,
            ..pinned(&[2, 10, 3, 11])
        };
        both.validate().expect("pinned, so both can be honoured");
    }

    #[test]
    fn three_threads_per_core_is_refused() {
        let mc = MachineConfig {
            threads_per_core: 3,
            ..pinned(&[0, 1, 2])
        };
        assert!(mc.validate().is_err());
    }

    #[test]
    fn hugepages_parse_and_default_to_transparent() {
        let mc: MachineConfig = serde_json::from_str(r#"{ "mem_size_mib": 4096 }"#).unwrap();
        assert_eq!(mc.hugepages, HugePages::Transparent);
        assert!(!mc.prefault);
        let mc: MachineConfig = serde_json::from_str(
            r#"{ "mem_size_mib": 4096, "hugepages": "1g", "prefault": true }"#,
        )
        .unwrap();
        assert_eq!(mc.hugepages, HugePages::Huge1G);
        assert!(mc.prefault);
        assert!(serde_json::from_str::<MachineConfig>(r#"{ "hugepages": "4k" }"#).is_err());
    }

    /// A pool page cannot be split, so RAM that ends mid-page cannot be backed.
    #[test]
    fn hugetlb_ram_must_be_whole_pages() {
        let with = |mem_size_mib, hugepages| MachineConfig {
            mem_size_mib,
            hugepages,
            ..MachineConfig::default()
        };
        with(4096, HugePages::Huge1G)
            .validate()
            .expect("four pages");
        assert!(with(3584, HugePages::Huge1G).validate().is_err());
        with(3584, HugePages::Huge2M)
            .validate()
            .expect("1792 pages");
        assert!(with(1025, HugePages::Huge2M).validate().is_err());
        with(1025, HugePages::Transparent)
            .validate()
            .expect("transparent pages impose nothing");
    }

    #[test]
    fn io_affinity_wins_over_the_guest_set_when_given() {
        let mc = MachineConfig {
            cpu_affinity: vec![2, 3],
            io_affinity: vec![0, 8],
            ..MachineConfig::default()
        };
        assert_eq!(mc.io_cpus(), &[0, 8]);
    }
}

#[cfg(test)]
mod drive_tests {
    use super::*;

    /// The cache and queue-count fields are new, and every config written
    /// before them omits them. Their absence has to keep meaning what it meant.
    #[test]
    fn a_drive_without_the_new_fields_still_parses() {
        let d: Drive = serde_json::from_str(
            r#"{"drive_id": "rootfs", "path_on_host": "/img.ext4", "is_root_device": true}"#,
        )
        .expect("parses");
        assert!(!d.is_read_only);
        // Absent, not false: absent means "direct where the host supports it",
        // and false means "do not, whatever the host supports".
        assert_eq!(d.direct, None);
        assert_eq!(d.num_queues, None);
    }

    #[test]
    fn the_cache_and_queue_fields_are_read_when_given() {
        let d: Drive = serde_json::from_str(
            r#"{"drive_id": "rootfs", "path_on_host": "/img.ext4", "is_root_device": true,
                "is_read_only": true, "direct": false, "num_queues": 2}"#,
        )
        .expect("parses");
        assert_eq!(d.direct, Some(false));
        assert_eq!(d.num_queues, Some(2));
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
        let net: Network = serde_json::from_str(r#"{ "tap-name": "nesbox1" }"#).expect("parses");
        assert_eq!(net.tap_name, "nesbox1");
        assert!(net.mac.is_none());
    }

    /// An exact name, because the host made the device. `%d` only ever worked
    /// when nesbox was the one creating it.
    #[test]
    fn a_tap_name_is_required() {
        assert!(serde_json::from_str::<Network>(r#"{ "mac": "02:00:00:00:00:01" }"#).is_err());
    }

    /// A key this build does not know is refused, not dropped.
    ///
    /// The case that prompted it: a config naming `gpu-forward` handed to a
    /// nesbox built before that device existed. It parsed, the key was
    /// dropped, and the guest booted with no GPU while the forwarding backend
    /// waited on a socket nobody connected to. The key here stands in for
    /// whatever the *next* such device is called -- this build knows
    /// `gpu-forward`, so using it would test nothing.
    #[test]
    fn a_config_key_this_build_does_not_know_is_refused() {
        let err = serde_json::from_str::<VmConfig>(
            r#"{ "machine-config": { "vcpu_count": 2 },
                 "some-device-a-later-build-adds": { "socket": "/tmp/x.sock" } }"#,
        )
        .expect_err("an unknown section must not be silently dropped");
        assert!(
            err.to_string().contains("some-device-a-later-build-adds"),
            "the error must name the key that was not understood, got: {err}"
        );
    }

    /// A misspelling is the same failure wearing different clothes, and is
    /// the commoner one.
    #[test]
    fn a_misspelled_key_is_refused_rather_than_ignored() {
        let err = serde_json::from_str::<VmConfig>(
            r#"{ "machine_config": { "vcpu_count": 2 } }"#,
        )
        .expect_err("machine_config is not machine-config");
        assert!(err.to_string().contains("machine_config"), "got: {err}");
    }

    /// And the same config against a build that does know it still parses, so
    /// the check above is catching the unknown key and not the shape.
    #[test]
    fn the_same_config_parses_where_the_device_exists() {
        let c: VmConfig = serde_json::from_str(
            r#"{ "machine-config": { "vcpu_count": 2 },
                 "gpu-forward": { "socket": "/tmp/nvgpu.sock" },
                 "shared-directories": [
                   { "tag": "nvidia", "path-on-host": "/var/lib/nvgpu", "read-only": true }
                 ] }"#,
        )
        .expect("every key here is one this build understands");
        assert_eq!(c.gpu_forward.expect("gpu-forward kept").socket,
                   PathBuf::from("/tmp/nvgpu.sock"));
        assert!(c.shared_directories[0].read_only);
    }
}
