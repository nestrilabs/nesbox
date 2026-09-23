//! Telling the guest the truth about its CPUs.
//!
//! KVM's `get_supported_cpuid` describes the *host* processor. Handing it to a
//! guest unaltered means a 7-vCPU sandbox on a 16-core host reports a package
//! containing 16 cores and 32 threads.
//!
//! Linux does not take its CPU count from there -- present CPUs come from the
//! ACPI MADT -- but it does derive `core_id`, `package_id` and the thread
//! sibling masks from these leaves. Given host values it computes sibling
//! relationships between vCPUs that share nothing, and builds its scheduling
//! domains on top of them. The result is a scheduler making SMT-aware
//! decisions about a topology that does not exist, which is the opposite of
//! what this guest wants.
//!
//! What is published instead: one package, one die, and either one thread per
//! core or two. One is honest for a set of whole cores inside a single L3
//! domain, with the host free to move vCPUs within it, and it means the guest
//! never believes two of its CPUs are hyperthread siblings. Two is honest only
//! when the caller pins each pair of vCPUs to two threads of one host core, and
//! it is what lets a guest with more vCPUs than cores spread work across cores
//! before doubling up.
//!
//! Modelled on Cloud Hypervisor's `update_cpuid_topology`
//! (`cloudhypervisor-for-llm-ref/arch/src/x86_64/mod.rs`), which is the
//! reference kept in this tree.

use kvm_bindings::{CpuId, kvm_cpuid_entry2};

/// Which vendor's extended leaves to patch. The topology leaves shared by both
/// are patched either way; only the `0x8000_001e`/`0x8000_0008` pair below is
/// AMD-specific.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Amd,
    Intel,
    Other,
}

/// Read the vendor string out of leaf 0.
pub fn vendor_of(cpuid: &CpuId) -> Vendor {
    match cpuid.as_slice().iter().find(|e| e.function == 0) {
        Some(e) => {
            // "AuthenticAMD" / "GenuineIntel", in EBX-EDX-ECX order.
            let mut s = [0u8; 12];
            s[0..4].copy_from_slice(&e.ebx.to_le_bytes());
            s[4..8].copy_from_slice(&e.edx.to_le_bytes());
            s[8..12].copy_from_slice(&e.ecx.to_le_bytes());
            match &s {
                b"AuthenticAMD" => Vendor::Amd,
                b"GenuineIntel" => Vendor::Intel,
                _ => Vendor::Other,
            }
        }
        None => Vendor::Other,
    }
}

/// Registers of one CPUID leaf.
#[derive(Clone, Copy)]
pub enum Reg {
    Eax,
    Ebx,
    Ecx,
    Edx,
}

/// Set one register of one (function, index) entry.
///
/// Entries absent from KVM's supported set are created, because a leaf the
/// host does not report is exactly the one a guest would otherwise read as
/// zero -- and a zeroed topology leaf is not "no topology", it is a leaf Linux
/// will parse and believe.
fn set(cpuid: &mut CpuId, function: u32, index: u32, reg: Reg, value: u32) {
    if let Some(e) = cpuid
        .as_mut_slice()
        .iter_mut()
        .find(|e| e.function == function && e.index == index)
    {
        match reg {
            Reg::Eax => e.eax = value,
            Reg::Ebx => e.ebx = value,
            Reg::Ecx => e.ecx = value,
            Reg::Edx => e.edx = value,
        }
        return;
    }

    let mut entry = kvm_cpuid_entry2 {
        function,
        index,
        flags: kvm_bindings::KVM_CPUID_FLAG_SIGNIFCANT_INDEX,
        ..Default::default()
    };
    match reg {
        Reg::Eax => entry.eax = value,
        Reg::Ebx => entry.ebx = value,
        Reg::Ecx => entry.ecx = value,
        Reg::Edx => entry.edx = value,
    }
    // A full table is not worth failing a boot over: the guest keeps the
    // host's value for this one leaf, which is what it would have had anyway.
    if cpuid.push(entry).is_err() {
        log::warn!("CPUID table full; leaf {function:#x} subleaf {index} left unpatched");
    }
}

fn get(cpuid: &CpuId, function: u32, index: u32, reg: Reg) -> u32 {
    cpuid
        .as_slice()
        .iter()
        .find(|e| e.function == function && e.index == index)
        .map(|e| match reg {
            Reg::Eax => e.eax,
            Reg::Ebx => e.ebx,
            Reg::Ecx => e.ecx,
            Reg::Edx => e.edx,
        })
        .unwrap_or(0)
}

/// Level types for leaves 0xb / 0x1f, as the SDM numbers them.
const LEVEL_INVALID: u32 = 0;
const LEVEL_SMT: u32 = 1;
const LEVEL_CORE: u32 = 2;

/// ECX of a topology subleaf: level number in bits 7:0, level type in 15:8.
///
/// The level number must equal the subleaf index that was asked for. Linux
/// ignores it and trusts the index it passed, but a guest that reads it should
/// not be told something false.
const fn topology_level(level: u32, level_type: u32) -> u32 {
    (level_type << 8) | level
}

/// Rewrite the topology leaves for one vCPU.
///
/// `vcpu_id` is also the x2APIC id, and the ids stay dense from zero so they
/// match the ACPI MADT entries built alongside them. With two threads per core
/// that still works: the SMT field is one bit wide, so id `2k` and `2k+1` are
/// threads 0 and 1 of core `k` with no gap to leave.
///
/// `threads_per_core` is 1 or 2. At 1 the guest is told every vCPU is a whole
/// core. At 2, vCPUs `2k` and `2k+1` are siblings, which is only true if the
/// caller pinned them to two threads of one host core.
pub fn patch_topology(
    cpuid: &mut CpuId,
    vcpu_id: u32,
    vcpu_count: u8,
    threads_per_core: u8,
    vendor: Vendor,
) {
    let logical = u32::from(vcpu_count.max(1));
    let threads = u32::from(threads_per_core.clamp(1, 2));
    let cores = logical.div_ceil(threads);
    let x2apic_id = vcpu_id;

    // Bits of the x2APIC id below the core index, and below the package.
    let thread_width = u32::BITS - (threads - 1).leading_zeros();
    let package_shift = thread_width + (u32::BITS - (cores - 1).leading_zeros());

    // ── Leaf 1: the oldest way to ask ────────────────────────────────────
    // EBX[23:16] is logical processors per package.
    let mut ebx = get(cpuid, 1, 0, Reg::Ebx);
    ebx &= !(0xff << 16);
    ebx |= (logical & 0xff) << 16;
    set(cpuid, 1, 0, Reg::Ebx, ebx);

    // EDX bit 28 (HTT) means "this package reports more than one logical
    // processor", not "hyperthreading is on". It has to be set for the field
    // above to be read at all.
    let edx = get(cpuid, 1, 0, Reg::Edx) | (1 << 28);
    set(cpuid, 1, 0, Reg::Edx, edx);

    // ── Leaves 0xb and 0x1f: extended topology ───────────────────────────
    // Both, because 0x1f takes precedence where it exists: leaving the host's
    // values there would let a guest that prefers it read the host's package
    // after 0xb told it the truth, and a half-patched pair is worse than
    // either alone.
    for leaf in [0xb, 0x1f] {
        // Level 0, SMT: how many threads a core has.
        set(cpuid, leaf, 0, Reg::Eax, thread_width);
        set(cpuid, leaf, 0, Reg::Ebx, threads);
        set(cpuid, leaf, 0, Reg::Ecx, topology_level(0, LEVEL_SMT));
        set(cpuid, leaf, 0, Reg::Edx, x2apic_id);

        // Level 1, core: every logical processor in the package.
        set(cpuid, leaf, 1, Reg::Eax, package_shift);
        set(cpuid, leaf, 1, Reg::Ebx, logical);
        set(cpuid, leaf, 1, Reg::Ecx, topology_level(1, LEVEL_CORE));
        set(cpuid, leaf, 1, Reg::Edx, x2apic_id);

        // A terminating subleaf. Enumeration stops at the first level of type
        // 0, and without one a guest walks off the end of what we defined into
        // whatever the host reported for subleaf 2.
        set(cpuid, leaf, 2, Reg::Eax, 0);
        set(cpuid, leaf, 2, Reg::Ebx, 0);
        set(cpuid, leaf, 2, Reg::Ecx, topology_level(2, LEVEL_INVALID));
        set(cpuid, leaf, 2, Reg::Edx, x2apic_id);
    }

    patch_cache_sharing(cpuid, logical, cores, threads, vendor);

    if vendor == Vendor::Amd {
        // 0x8000_0008 ECX[7:0] is "number of threads in the package, minus
        // one", and Linux's AMD topology path reads it before falling back to
        // 0xb.
        let mut ecx = get(cpuid, 0x8000_0008, 0, Reg::Ecx);
        ecx &= !0xff;
        ecx |= (logical - 1) & 0xff;
        set(cpuid, 0x8000_0008, 0, Reg::Ecx, ecx);

        // 0x8000_001e: extended APIC id; EBX[7:0] is the core id and EBX[15:8]
        // threads per core, minus one.
        set(cpuid, 0x8000_001e, 0, Reg::Eax, x2apic_id);
        let core_id = (x2apic_id >> thread_width) & 0xff;
        set(
            cpuid,
            0x8000_001e,
            0,
            Reg::Ebx,
            ((threads - 1) << 8) | core_id,
        );
        // Node id 0, one node per processor.
        set(cpuid, 0x8000_001e, 0, Reg::Ecx, 0);
        set(cpuid, 0x8000_001e, 0, Reg::Edx, 0);
    }
}

/// `KVM_CPUID_FEATURES`, and the hint in its EDX that says a vCPU owns its CPU.
const KVM_CPUID_SIGNATURE: u32 = 0x4000_0000;
const KVM_CPUID_FEATURES: u32 = 0x4000_0001;
const KVM_HINTS_REALTIME: u32 = 1 << 0;

/// Tell the guest its vCPUs are never preempted.
///
/// The guest then uses plain spinlocks rather than paravirtual ones and loads
/// its polling idle driver. Right only for a vCPU pinned to a CPU nothing else
/// runs on; see `MachineConfig::dedicated`.
///
/// Returns false, and changes nothing, when KVM's leaves are not there to
/// carry it: the hint is meaningless to a guest that cannot see it is on KVM.
pub fn advertise_dedicated(cpuid: &mut CpuId) -> bool {
    if get(cpuid, KVM_CPUID_SIGNATURE, 0, Reg::Eax) < KVM_CPUID_FEATURES {
        return false;
    }
    let edx = get(cpuid, KVM_CPUID_FEATURES, 0, Reg::Edx) | KVM_HINTS_REALTIME;
    set(cpuid, KVM_CPUID_FEATURES, 0, Reg::Edx, edx);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for what KVM hands us on a 16-core / 32-thread host: leaf 1
    /// claiming 32 logical processors, and leaf 0xb describing two threads per
    /// core. Every assertion below is really "this host value did not survive".
    pub fn host_like_cpuid() -> CpuId {
        CpuId::from_entries(&[
            kvm_cpuid_entry2 {
                function: 0,
                ebx: u32::from_le_bytes(*b"Auth"),
                edx: u32::from_le_bytes(*b"enti"),
                ecx: u32::from_le_bytes(*b"cAMD"),
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 1,
                ebx: 32 << 16,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0xb,
                index: 0,
                eax: 1,
                ebx: 2,
                ecx: 1 << 8,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0xb,
                index: 1,
                eax: 5,
                ebx: 32,
                ecx: 2 << 8,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0x8000_0008,
                ecx: 15,
                ..Default::default()
            },
            // Host cache leaves: L1d and L2 shared by two logical CPUs (the
            // host's SMT pair), L3 shared by sixteen.
            kvm_cpuid_entry2 {
                function: 0x8000_001d,
                index: 0,
                eax: 1 | (1 << 5) | (1 << 14), // data, level 1, sharing 2
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0x8000_001d,
                index: 2,
                eax: 3 | (2 << 5) | (1 << 14), // unified, level 2, sharing 2
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0x8000_001d,
                index: 3,
                eax: 3 | (3 << 5) | (15 << 14), // unified, level 3, sharing 16
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0x8000_001d,
                index: 4,
                eax: 0, // terminator
                ..Default::default()
            },
        ])
        .expect("builds")
    }

    fn reg(cpuid: &CpuId, function: u32, index: u32, r: Reg) -> u32 {
        get(cpuid, function, index, r)
    }

    #[test]
    fn the_vendor_string_is_read_from_leaf_0() {
        assert_eq!(vendor_of(&host_like_cpuid()), Vendor::Amd);
    }

    /// Two threads per core: the SMT level is one bit wide and two threads
    /// deep, and the package holds every vCPU.
    #[test]
    fn with_two_threads_per_core_the_smt_level_says_so() {
        let mut c = host_like_cpuid();
        patch_topology(&mut c, 3, 6, 2, Vendor::Amd);
        for leaf in [0xb, 0x1f] {
            assert_eq!(reg(&c, leaf, 0, Reg::Eax), 1, "one bit of thread id");
            assert_eq!(reg(&c, leaf, 0, Reg::Ebx), 2, "two threads a core");
            // Six logical ids, 0..=5, need three bits below the package.
            assert_eq!(reg(&c, leaf, 1, Reg::Eax), 3);
            assert_eq!(reg(&c, leaf, 1, Reg::Ebx), 6);
            assert_eq!(reg(&c, leaf, 1, Reg::Edx), 3, "x2APIC id is the vCPU index");
        }
        assert_eq!((reg(&c, 1, 0, Reg::Ebx) >> 16) & 0xff, 6);
    }

    /// vCPUs `2k` and `2k+1` are the two threads of core `k`, on AMD's own
    /// topology leaf too: Linux reads the core id from there before 0xb.
    #[test]
    fn amd_pairs_vcpus_into_cores() {
        for (vcpu, core) in [(0, 0), (1, 0), (2, 1), (3, 1), (4, 2)] {
            let mut c = host_like_cpuid();
            patch_topology(&mut c, vcpu, 6, 2, Vendor::Amd);
            let ebx = reg(&c, 0x8000_001e, 0, Reg::Ebx);
            assert_eq!(ebx & 0xff, core, "vCPU {vcpu}");
            assert_eq!((ebx >> 8) & 0xff, 1, "two threads, minus one");
            assert_eq!(reg(&c, 0x8000_0008, 0, Reg::Ecx) & 0xff, 5, "six threads");
        }
    }

    /// An odd count leaves the last core with one thread rather than inventing
    /// a vCPU to fill it.
    #[test]
    fn an_odd_count_with_siblings_rounds_the_core_count_up() {
        let mut c = host_like_cpuid();
        patch_topology(&mut c, 4, 5, 2, Vendor::Intel);
        assert_eq!(reg(&c, 0xb, 1, Reg::Ebx), 5);
        // Five ids need three bits: one of thread, two of core (three cores).
        assert_eq!(reg(&c, 0xb, 1, Reg::Eax), 3);
    }

    #[test]
    fn the_dedicated_hint_needs_kvm_leaves_to_carry_it() {
        let mut c = host_like_cpuid();
        assert!(!advertise_dedicated(&mut c), "no KVM signature, no hint");
        assert_eq!(reg(&c, KVM_CPUID_FEATURES, 0, Reg::Edx), 0);

        set(&mut c, KVM_CPUID_SIGNATURE, 0, Reg::Eax, KVM_CPUID_FEATURES);
        set(&mut c, KVM_CPUID_FEATURES, 0, Reg::Edx, 1 << 3);
        assert!(advertise_dedicated(&mut c));
        assert_eq!(
            reg(&c, KVM_CPUID_FEATURES, 0, Reg::Edx),
            (1 << 3) | KVM_HINTS_REALTIME,
            "the hint is added, nothing already there is lost"
        );
    }

    #[test]
    fn leaf_1_reports_the_guest_core_count_not_the_host() {
        let mut c = host_like_cpuid();
        patch_topology(&mut c, 0, 7, 1, Vendor::Amd);
        assert_eq!(
            (reg(&c, 1, 0, Reg::Ebx) >> 16) & 0xff,
            7,
            "host's 32 survived"
        );
        assert_ne!(reg(&c, 1, 0, Reg::Edx) & (1 << 28), 0, "HTT must be set");
    }

    /// The whole point: the guest must never believe two of its CPUs share a
    /// physical core, because the caller hands it whole cores.
    #[test]
    fn no_two_vcpus_are_hyperthread_siblings() {
        let mut c = host_like_cpuid();
        patch_topology(&mut c, 0, 7, 1, Vendor::Amd);
        for leaf in [0xb, 0x1f] {
            assert_eq!(
                reg(&c, leaf, 0, Reg::Ebx),
                1,
                "leaf {leaf:#x}: threads per core"
            );
            assert_eq!(reg(&c, leaf, 0, Reg::Eax), 0, "leaf {leaf:#x}: SMT shift");
            assert_eq!(reg(&c, leaf, 0, Reg::Ecx) >> 8 & 0xff, 1, "level type SMT");
        }
    }

    #[test]
    fn the_core_level_covers_every_vcpu() {
        let mut c = host_like_cpuid();
        patch_topology(&mut c, 0, 7, 1, Vendor::Amd);
        for leaf in [0xb, 0x1f] {
            assert_eq!(reg(&c, leaf, 1, Reg::Ebx), 7);
            // 3 bits hold 0..=7.
            assert_eq!(reg(&c, leaf, 1, Reg::Eax), 3, "core index width");
            assert_eq!(reg(&c, leaf, 1, Reg::Ecx) >> 8 & 0xff, 2, "level type Core");
            assert_eq!(
                reg(&c, leaf, 1, Reg::Ecx) & 0xff,
                1,
                "level number matches subleaf"
            );
        }
    }

    /// Enumeration stops at the first level of type 0. Without a terminator a
    /// guest reads whatever the host left in subleaf 2.
    #[test]
    fn enumeration_terminates() {
        let mut c = host_like_cpuid();
        patch_topology(&mut c, 0, 7, 1, Vendor::Amd);
        for leaf in [0xb, 0x1f] {
            assert_eq!(
                reg(&c, leaf, 2, Reg::Ecx) >> 8 & 0xff,
                0,
                "leaf {leaf:#x} terminator"
            );
            assert_eq!(reg(&c, leaf, 2, Reg::Ebx), 0);
        }
    }

    #[test]
    fn each_vcpu_gets_its_own_x2apic_id() {
        for id in 0..7u32 {
            let mut c = host_like_cpuid();
            patch_topology(&mut c, id, 7, 1, Vendor::Amd);
            assert_eq!(reg(&c, 0xb, 1, Reg::Edx), id);
            assert_eq!(reg(&c, 0x8000_001e, 0, Reg::Eax), id);
        }
    }

    #[test]
    fn amd_extended_leaves_are_patched_and_intel_is_left_alone() {
        let mut amd = host_like_cpuid();
        patch_topology(&mut amd, 0, 7, 1, Vendor::Amd);
        assert_eq!(
            reg(&amd, 0x8000_0008, 0, Reg::Ecx) & 0xff,
            6,
            "cores minus one"
        );
        assert_eq!(
            reg(&amd, 0x8000_001e, 0, Reg::Ebx) >> 8 & 0xff,
            0,
            "one thread per unit"
        );

        let mut intel = host_like_cpuid();
        patch_topology(&mut intel, 0, 7, 1, Vendor::Intel);
        assert_eq!(
            reg(&intel, 0x8000_0008, 0, Reg::Ecx) & 0xff,
            15,
            "AMD-only leaf touched"
        );
    }

    /// A single-vCPU guest is the degenerate case the width arithmetic is
    /// easiest to get wrong on: `(1 - 1).leading_zeros()` is 32, not 31.
    #[test]
    fn a_single_vcpu_guest_is_one_core_zero_width() {
        let mut c = host_like_cpuid();
        patch_topology(&mut c, 0, 1, 1, Vendor::Amd);
        assert_eq!(reg(&c, 0xb, 1, Reg::Eax), 0, "no bits needed for one core");
        assert_eq!(reg(&c, 0xb, 1, Reg::Ebx), 1);
        assert_eq!((reg(&c, 1, 0, Reg::Ebx) >> 16) & 0xff, 1);
    }

    /// Leaves KVM never reported must be created, not skipped: a guest reading
    /// zeros from 0x1f would parse them as a real topology.
    #[test]
    fn absent_leaves_are_created() {
        let mut c = host_like_cpuid();
        assert_eq!(
            c.as_slice().iter().filter(|e| e.function == 0x1f).count(),
            0
        );
        patch_topology(&mut c, 0, 7, 1, Vendor::Amd);
        assert_eq!(
            c.as_slice().iter().filter(|e| e.function == 0x1f).count(),
            3
        );
    }
}

/// Rewrite how many logical processors share each cache.
///
/// The topology leaves above describe cores; these describe caches, and Linux
/// uses both. `cacheinfo` builds each CPU's shared-cpu mask from here, and the
/// scheduler turns the last-level one into its LLC domain. Left at host values
/// a guest reads its L1 and L2 as shared between pairs of CPUs -- the host's
/// SMT siblings -- and concludes that CPUs 0 and 1 sit on one physical core
/// after leaf 0xb just told it they do not. `lscpu` shows it as half as many
/// cache instances as there are cores.
///
/// AMD's `0x8000_001d` and Intel's leaf 4 carry the field at the same place:
/// EAX[25:14] is the count of logical processors sharing the cache, minus one.
///
/// L1 and L2 become private to their core, shared by its threads when it has
/// two. The last level stays shared by every vCPU, which is true by
/// construction when a guest is placed inside a single L3 domain, so all of
/// its CPUs really do share one.
fn patch_cache_sharing(cpuid: &mut CpuId, logical: u32, cores: u32, threads: u32, vendor: Vendor) {
    let leaf = match vendor {
        Vendor::Amd => 0x8000_001d,
        Vendor::Intel => 4,
        // Some other vendor's cache enumeration is not something to guess at;
        // leaving it alone is worse than wrong only if it is also read, and a
        // wrong guess is wrong either way.
        Vendor::Other => return,
    };

    // Collect first: the entries are borrowed immutably while scanning.
    let subleaves: Vec<(u32, u32)> = cpuid
        .as_slice()
        .iter()
        .filter(|e| e.function == leaf)
        .map(|e| (e.index, e.eax))
        .collect();

    for (index, eax) in subleaves {
        let cache_type = eax & 0x1f;
        // Type 0 terminates the enumeration; nothing past it is a cache.
        if cache_type == 0 {
            continue;
        }
        let level = (eax >> 5) & 0x7;

        // Everything below the last level is private to one core, and so
        // shared only by that core's threads.
        let sharing = if level >= 3 { logical } else { threads };

        let mut new_eax = eax & !(0xfff << 14);
        new_eax |= ((sharing - 1) & 0xfff) << 14;

        // Intel additionally puts "cores in this package, minus one" in
        // EAX[31:26]. AMD reserves those bits, so only Intel gets it.
        if vendor == Vendor::Intel {
            new_eax &= !(0x3f << 26);
            new_eax |= ((cores - 1) & 0x3f) << 26;
        }

        set(cpuid, leaf, index, Reg::Eax, new_eax);
    }
}

#[cfg(test)]
mod cache_tests {
    use super::tests::*;
    use super::*;

    fn sharing(cpuid: &CpuId, index: u32) -> u32 {
        ((get(cpuid, 0x8000_001d, index, Reg::Eax) >> 14) & 0xfff) + 1
    }

    /// `lscpu` reporting four cache instances for eight cores is what this
    /// prevents: the host's SMT pairing leaking in after leaf 0xb said there
    /// are no siblings.
    #[test]
    fn low_level_caches_become_private_to_their_core() {
        let mut c = host_like_cpuid();
        assert_eq!(sharing(&c, 0), 2, "fixture starts SMT-shared");
        patch_topology(&mut c, 0, 8, 1, Vendor::Amd);
        assert_eq!(sharing(&c, 0), 1, "L1d must be private");
        assert_eq!(sharing(&c, 2), 1, "L2 must be private");
    }

    /// With siblings, L1 and L2 are shared by exactly the two threads of a core
    /// -- the host's own pairing, which pinning made true in the guest.
    #[test]
    fn with_siblings_low_level_caches_are_shared_by_the_pair() {
        let mut c = host_like_cpuid();
        patch_topology(&mut c, 0, 6, 2, Vendor::Amd);
        assert_eq!(sharing(&c, 0), 2);
        assert_eq!(sharing(&c, 2), 2);
        assert_eq!(sharing(&c, 3), 6, "L3 is still every vCPU");
    }

    /// The last level really is shared by every vCPU -- a guest is placed inside
    /// one L3 domain -- so it should say so, for this guest's size and not the
    /// host's.
    #[test]
    fn the_last_level_is_shared_by_every_vcpu() {
        let mut c = host_like_cpuid();
        patch_topology(&mut c, 0, 8, 1, Vendor::Amd);
        assert_eq!(sharing(&c, 3), 8);
    }

    /// The type and level fields identify the cache; only the sharing count
    /// should move.
    #[test]
    fn cache_type_and_level_are_left_alone() {
        let mut c = host_like_cpuid();
        patch_topology(&mut c, 0, 8, 1, Vendor::Amd);
        let eax = get(&c, 0x8000_001d, 3, Reg::Eax);
        assert_eq!(eax & 0x1f, 3, "still unified");
        assert_eq!((eax >> 5) & 0x7, 3, "still level 3");
    }

    #[test]
    fn the_terminator_is_not_treated_as_a_cache() {
        let mut c = host_like_cpuid();
        patch_topology(&mut c, 0, 8, 1, Vendor::Amd);
        assert_eq!(get(&c, 0x8000_001d, 4, Reg::Eax), 0, "terminator rewritten");
    }

    /// AMD reserves EAX[31:26]; only Intel's leaf 4 carries a core count there.
    #[test]
    fn intel_uses_leaf_4_and_amd_leaf_is_untouched() {
        let mut c = host_like_cpuid();
        patch_topology(&mut c, 0, 8, 1, Vendor::Intel);
        assert_eq!(sharing(&c, 0), 2, "AMD leaf must be left alone for Intel");
    }
}
