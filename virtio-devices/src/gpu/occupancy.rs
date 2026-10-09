// GPU occupancy for this process, from the kernel's per-client DRM accounting.
//
// One VMM process is one DRM client, so per-process is per-guest.
//
// # Why fdinfo and not fences
//
// A fence measures submit-to-signal *latency*, which with more than one guest on
// the card includes time queued behind another guest's work. `drm-engine-gfx`
// measures *occupancy* — nanoseconds the engine actually spent on this client.
// Solo the two agree, which is exactly how confusing them survives a
// single-guest experiment and then misleads a multi-guest one.
//
// Occupancy is what admission needs: it is the numerator of `U`, the fraction of
// a card a guest is consuming. Latency is what a player feels. Both matter and
// they are not interchangeable.
//
// # Memory, across drivers
//
// The kernel's common fdinfo keys are per memory region, `drm-total-<region>`
// and `drm-resident-<region>`, and every driver names its regions its own way:
// amdgpu `vram` / `gtt` / `cpu`, i915 `local0` / `system0` (and `stolen-*`),
// xe `vram0` / `gtt` / `system`. Device memory is the regions named `vram*`
// or `local*`; host memory is `gtt*`, `system*` and `cpu`. Stolen memory is
// carved out by firmware and belongs to neither.
//
// `drm-total` counts buffers where the driver wants them; `drm-resident` where
// their pages are. What device memory is total but not resident means a
// different thing per driver, which is why the driver is reported with it:
//
// - **amdgpu** places a buffer when it is created and counts it under its
//   preferred domain, so device memory not resident is device memory spilled
//   to system memory -- exactly. RADV asks for VRAM *or* GTT by default, which
//   amdgpu counts as VRAM, so this sees spills `amd-evicted-vram` does not.
// - **i915** gives a buffer pages when it is first used, so the same number
//   also holds memory nothing has touched yet. An upper bound on the spill.
// - **xe**: not yet measured.
//
// # Why re-resolve the fd every read
//
// The DRM client fd is whichever one advertises an engine counter, and it does
// not exist until the renderer has opened the render node and the guest has
// created a context. It can also vanish and reappear. So the fd number is cached
// as a hint and re-resolved whenever the hint stops carrying counters, which is a
// lesson the sampling script learned first.

use std::fs;
use std::sync::Mutex;

/// Which kernel driver the client belongs to, from `drm-driver`. Says how to
/// read the memory numbers; see the module's notes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Driver {
    Amdgpu,
    I915,
    Xe,
    #[default]
    Other,
}

impl Driver {
    fn from_name(name: &str) -> Self {
        match name {
            "amdgpu" => Self::Amdgpu,
            "i915" => Self::I915,
            "xe" => Self::Xe,
            _ => Self::Other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Amdgpu => "amdgpu",
            Self::I915 => "i915",
            Self::Xe => "xe",
            Self::Other => "other",
        }
    }
}

/// Where a memory region's bytes are, by its fdinfo name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Region {
    Device,
    Host,
}

fn region(name: &str) -> Option<Region> {
    if name.starts_with("stolen") {
        None
    } else if name.starts_with("vram") || name.starts_with("local") {
        Some(Region::Device)
    } else if name.starts_with("gtt") || name.starts_with("system") || name == "cpu" {
        Some(Region::Host)
    } else {
        None
    }
}

/// One sample of what the card has spent on this client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Occupancy {
    pub driver: Driver,
    /// Whether the driver reports engine time in nanoseconds, under a name this
    /// reader knows. Without it `gfx_ns` and `compute_ns` are unknown, not
    /// zero: xe, for one, reports engine cycles instead.
    pub engine_time: bool,
    /// Nanoseconds the graphics engine has spent on this client, monotonic since
    /// the client opened. A rate is two samples and the wall time between them.
    pub gfx_ns: u64,
    /// The compute engine's counterpart. A guest's async compute -- and the
    /// capture layer's own conversion passes -- run here, not on `gfx_ns`.
    pub compute_ns: u64,
    /// What the client has asked the card for.
    pub requested_vram_bytes: u64,
    /// What is actually in VRAM now. Lower than requested means amdgpu has
    /// migrated buffers out to GTT under us.
    pub resident_vram_bytes: u64,
    /// Non-zero means this guest's quota is above what the card will really give
    /// it, and it is paying for the difference in bus traffic.
    pub evicted_vram_bytes: u64,
    /// Device memory the driver counts this client for, in every device region.
    pub device_total_bytes: u64,
    /// The part of it that is in device memory now.
    pub device_resident_bytes: u64,
    /// Host memory the GPU reads for this client: GTT and system regions.
    pub host_resident_bytes: u64,
}

impl Occupancy {
    /// Device memory counted but not resident. On amdgpu, spilled to system
    /// memory; on i915 also untouched; see the module's notes.
    pub fn device_not_resident_bytes(&self) -> u64 {
        self.device_total_bytes
            .saturating_sub(self.device_resident_bytes)
    }
}

/// Reads this process's DRM client accounting.
pub struct OccupancyReader {
    /// Last fd number that carried engine counters. A hint, not a fact.
    fd_hint: Mutex<Option<String>>,
}

impl OccupancyReader {
    pub fn new() -> Self {
        Self {
            fd_hint: Mutex::new(None),
        }
    }

    /// A sample, or `None` if this process has no DRM client yet — which is the
    /// normal state until the guest creates its first GPU context.
    pub fn read(&self) -> Option<Occupancy> {
        let mut hint = self.fd_hint.lock().unwrap();

        if let Some(fd) = hint.as_deref() {
            if let Some(o) = Self::parse(&format!("/proc/self/fdinfo/{fd}")) {
                return Some(o);
            }
        }

        // The hint is stale or absent. Scan.
        let entries = fs::read_dir("/proc/self/fdinfo").ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(o) = Self::parse(path.to_str()?) {
                *hint = entry.file_name().to_str().map(str::to_owned);
                return Some(o);
            }
        }
        *hint = None;
        None
    }

    /// `None` unless this fdinfo names a DRM client, which is what makes a DRM
    /// client fd recognisable among a process's sockets, files and eventfds.
    /// `drm-client-id` rather than an engine counter: every driver writes it,
    /// and each names its engines its own way.
    fn parse(path: &str) -> Option<Occupancy> {
        let text = fs::read_to_string(path).ok()?;
        let mut o = Occupancy::default();
        let mut is_drm_client = false;

        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key {
                "drm-client-id" => is_drm_client = true,
                "drm-driver" => o.driver = Driver::from_name(value),
                // "<n> ns". amdgpu's graphics engine is `gfx`, i915's `render`.
                "drm-engine-gfx" | "drm-engine-render" => {
                    o.gfx_ns = value.split_whitespace().next()?.parse().ok()?;
                    o.engine_time = true;
                }
                "drm-engine-compute" => {
                    o.compute_ns = value.split_whitespace().next()?.parse().ok()?;
                }
                "amd-requested-vram" => o.requested_vram_bytes = parse_kib(value)?,
                "amd-evicted-vram" => o.evicted_vram_bytes = parse_kib(value)?,
                _ => {
                    if key == "drm-resident-vram" {
                        o.resident_vram_bytes = parse_kib(value)?;
                    }
                    if let Some(name) = key.strip_prefix("drm-total-") {
                        if region(name) == Some(Region::Device) {
                            o.device_total_bytes += parse_kib(value)?;
                        }
                    } else if let Some(name) = key.strip_prefix("drm-resident-") {
                        match region(name) {
                            Some(Region::Device) => o.device_resident_bytes += parse_kib(value)?,
                            Some(Region::Host) => o.host_resident_bytes += parse_kib(value)?,
                            None => {}
                        }
                    }
                }
            }
        }

        is_drm_client.then_some(o)
    }
}

impl Default for OccupancyReader {
    fn default() -> Self {
        Self::new()
    }
}

/// DRM memory lines are "<n> KiB". Anything else is a kernel we do not know, and
/// guessing at the unit would be worse than reporting nothing.
fn parse_kib(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let n: u64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some("KiB") | None => Some(n * 1024),
        Some("MiB") => Some(n * 1024 * 1024),
        Some("B") => Some(n),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, body: &str) -> String {
        let path = std::env::temp_dir().join(name);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path.to_str().unwrap().to_owned()
    }

    /// Shape taken from a real amdgpu client on the reference host.
    const REAL: &str = "\
pos:\t0
flags:\t02100002
mnt_id:\t26
ino:\t1234
drm-driver:\tamdgpu
drm-client-id:\t42
drm-pdev:\t0000:04:00.0
drm-engine-gfx:\t48123456789 ns
drm-engine-compute:\t1234567 ns
drm-memory-vram:\t37584 KiB
amd-requested-vram:\t37584 KiB
drm-resident-vram:\t37584 KiB
amd-evicted-vram:\t0 KiB
";

    #[test]
    fn parses_a_real_amdgpu_client() {
        let p = write_tmp("nesbox-fdinfo-real", REAL);
        let o = OccupancyReader::parse(&p).expect("should recognise a DRM client");
        assert_eq!(o.gfx_ns, 48_123_456_789);
        assert_eq!(o.compute_ns, 1_234_567);
        assert_eq!(o.requested_vram_bytes, 37584 * 1024);
        assert_eq!(o.resident_vram_bytes, 37584 * 1024);
        assert_eq!(o.evicted_vram_bytes, 0);
    }

    #[test]
    fn a_non_drm_fd_is_not_mistaken_for_one() {
        // A socket's fdinfo. Without the engine counter there is nothing to
        // report, and reporting zeroes would look like an idle GPU.
        let p = write_tmp("nesbox-fdinfo-sock", "pos:\t0\nflags:\t02\nmnt_id:\t9\n");
        assert!(OccupancyReader::parse(&p).is_none());
    }

    /// A spill on amdgpu: buffers counted as VRAM, some of them resident in
    /// GTT. Shape from a box on nestripc-1 that had spilled.
    #[test]
    fn an_amdgpu_spill_is_device_memory_not_resident() {
        let body = "drm-driver:\tamdgpu\ndrm-client-id:\t7\n\
                    drm-engine-gfx:\t5 ns\n\
                    drm-total-vram:\t10485760 KiB\ndrm-resident-vram:\t8388608 KiB\n\
                    drm-total-gtt:\t3145728 KiB\ndrm-resident-gtt:\t3145728 KiB\n\
                    drm-total-cpu:\t0 KiB\ndrm-resident-cpu:\t0 KiB\n";
        let p = write_tmp("nesbox-fdinfo-spill", body);
        let o = OccupancyReader::parse(&p).unwrap();
        assert_eq!(o.driver, Driver::Amdgpu);
        assert_eq!(o.device_total_bytes, 10 << 30);
        assert_eq!(o.device_resident_bytes, 8 << 30);
        assert_eq!(o.device_not_resident_bytes(), 2 << 30);
        assert_eq!(o.host_resident_bytes, 3 << 30);
    }

    /// i915, from an Arc A310: other region names, `render` for the graphics
    /// engine, stolen memory left out, and recognised with no `gfx` counter.
    #[test]
    fn an_i915_client_is_read_with_its_own_names() {
        let body = "drm-driver:\ti915\ndrm-client-id:\t2704\ndrm-pdev:\t0000:03:00.0\n\
                    drm-total-system0:\t8012 KiB\ndrm-resident-system0:\t520 KiB\n\
                    drm-total-local0:\t35916 KiB\ndrm-shared-local0:\t32 MiB\n\
                    drm-resident-local0:\t12 KiB\n\
                    drm-total-stolen-local0:\t64 KiB\ndrm-resident-stolen-local0:\t64 KiB\n\
                    drm-engine-render:\t51781489656 ns\ndrm-engine-compute:\t82420 ns\n";
        let p = write_tmp("nesbox-fdinfo-i915", body);
        let o = OccupancyReader::parse(&p).expect("an i915 client is a DRM client");
        assert_eq!(o.driver, Driver::I915);
        assert!(o.engine_time);
        assert_eq!(o.gfx_ns, 51_781_489_656);
        assert_eq!(o.compute_ns, 82_420);
        assert_eq!(o.device_total_bytes, 35916 * 1024);
        assert_eq!(o.device_resident_bytes, 12 * 1024);
        assert_eq!(o.host_resident_bytes, 520 * 1024);
    }

    #[test]
    fn eviction_is_carried_through() {
        let body = REAL.replace("amd-evicted-vram:\t0 KiB", "amd-evicted-vram:\t8192 KiB");
        let p = write_tmp("nesbox-fdinfo-evict", &body);
        let o = OccupancyReader::parse(&p).unwrap();
        assert_eq!(o.evicted_vram_bytes, 8 * 1024 * 1024);
    }

    #[test]
    fn an_unknown_unit_reports_nothing_rather_than_a_wrong_number() {
        assert_eq!(parse_kib("100 KiB"), Some(102_400));
        assert_eq!(parse_kib("2 MiB"), Some(2 * 1024 * 1024));
        assert_eq!(parse_kib("512 B"), Some(512));
        assert_eq!(parse_kib("7 furlongs"), None);
        assert_eq!(parse_kib("not-a-number KiB"), None);
    }

    /// Some drivers report memory without engine time in nanoseconds. Still a
    /// client, for its memory; its engine time is unknown rather than zero,
    /// which would read as an idle GPU.
    #[test]
    fn a_missing_engine_counter_is_unknown_engine_time_not_an_idle_gpu() {
        let body = REAL.replace("drm-engine-gfx:\t48123456789 ns\n", "");
        let p = write_tmp("nesbox-fdinfo-nogfx", &body);
        let o = OccupancyReader::parse(&p).expect("still a DRM client");
        assert!(!o.engine_time);
        assert_eq!(o.resident_vram_bytes, 37584 * 1024);
    }

    #[test]
    fn reading_this_process_does_not_panic_and_finds_no_gpu() {
        // The test binary has no DRM client. The contract is None, not zeroes.
        assert!(OccupancyReader::new().read().is_none());
    }
}
