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
// # Why re-resolve the fd every read
//
// The DRM client fd is whichever one advertises an engine counter, and it does
// not exist until the renderer has opened the render node and the guest has
// created a context. It can also vanish and reappear. So the fd number is cached
// as a hint and re-resolved whenever the hint stops carrying counters, which is a
// lesson the sampling script learned first.

use std::fs;
use std::sync::Mutex;

/// One sample of what the card has spent on this client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Occupancy {
    /// Nanoseconds the graphics engine has spent on this client, monotonic since
    /// the client opened. A rate is two samples and the wall time between them.
    pub gfx_ns: u64,
    /// What the client has asked the card for.
    pub requested_vram_bytes: u64,
    /// What is actually in VRAM now. Lower than requested means amdgpu has
    /// migrated buffers out to GTT under us.
    pub resident_vram_bytes: u64,
    /// Non-zero means this guest's quota is above what the card will really give
    /// it, and it is paying for the difference in bus traffic.
    pub evicted_vram_bytes: u64,
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

    /// `None` unless this fdinfo carries an engine counter, which is what makes a
    /// DRM client fd recognisable among a process's sockets, files and eventfds.
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
                "drm-engine-gfx" => {
                    // "<n> ns"
                    o.gfx_ns = value.split_whitespace().next()?.parse().ok()?;
                    is_drm_client = true;
                }
                "amd-requested-vram" => o.requested_vram_bytes = parse_kib(value)?,
                "drm-resident-vram" => o.resident_vram_bytes = parse_kib(value)?,
                "amd-evicted-vram" => o.evicted_vram_bytes = parse_kib(value)?,
                _ => {}
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

    #[test]
    fn a_missing_engine_counter_disqualifies_even_with_memory_lines() {
        // Some DRM drivers report memory without per-engine time. Occupancy is
        // the point, so this is not a client we can meter.
        let body = REAL.replace("drm-engine-gfx:\t48123456789 ns\n", "");
        let p = write_tmp("nesbox-fdinfo-nogfx", &body);
        assert!(OccupancyReader::parse(&p).is_none());
    }

    #[test]
    fn reading_this_process_does_not_panic_and_finds_no_gpu() {
        // The test binary has no DRM client. The contract is None, not zeroes.
        assert!(OccupancyReader::new().read().is_none());
    }
}
