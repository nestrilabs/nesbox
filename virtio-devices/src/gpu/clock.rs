// How fast the card's shader clock is running, against the clock it promises.
//
// # Why the GPU time limit needs it
//
// Engine time is how long the card was busy with a guest, not how much it did.
// At half the clock a frame takes twice as long, so a guest held to a quarter
// of the engine's *time* gets an eighth of the card. And the card picks its
// clock from how busy it looks: guests held back leave it idle in between, so
// it settles low -- measured, three guests at 25% each held a 160 W card at
// 1.7 GHz and 61 W. The limit was starving the card of the load that would
// have raised its clock.
//
// So time is charged as work: engine time scaled by the clock it ran at,
// against a fixed reference. A guest on a slow clock may use more time; the
// card then looks busy, raises its clock, and the guest's time falls back.
//
// # The reference
//
// The last level `pp_dpm_sclk` reports. amdgpu deliberately lowers that to the
// clock the firmware guarantees every card of the SKU reaches
// (`DriverReportedClocks.GameClockAc` on SMU 14), which real boost may exceed.
// That is the right number to promise against: a share of a card running at a
// clock every card of the model reaches. A guest on a card boosting past it
// pays slightly more than its engine time.
//
// It is scaled by the shader clock alone. Work bound by memory does not slow
// down with it, so it is charged a little less than it costs; the direction is
// still right, which is what the feedback needs.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How old a clock reading may be before it is read again. The card changes
/// clock on this order; the limiter looks far more often than that, and every
/// read is a trip to the card's firmware.
const FRESH: Duration = Duration::from_millis(10);

pub struct ShaderClock {
    /// hwmon's `freq1_input` for the shader clock, in Hz.
    input: PathBuf,
    reference_hz: u64,
    last: Mutex<(Instant, u64)>,
}

impl ShaderClock {
    /// The clock of the card behind `render_node`, or `None` for one that does
    /// not report one -- whose guests are then charged plain time.
    pub fn for_render_node(render_node: &Path) -> Option<Self> {
        let device = Path::new("/sys/class/drm")
            .join(render_node.file_name()?)
            .join("device");
        let reference_hz =
            reference_hz(&std::fs::read_to_string(device.join("pp_dpm_sclk")).ok()?)?;
        let input = std::fs::read_dir(device.join("hwmon"))
            .ok()?
            .flatten()
            .map(|hwmon| hwmon.path())
            .find(|hwmon| {
                std::fs::read_to_string(hwmon.join("freq1_label"))
                    .is_ok_and(|label| label.trim() == "sclk")
            })?
            .join("freq1_input");
        let now = read_hz(&input)?;
        Some(Self {
            input,
            reference_hz,
            last: Mutex::new((Instant::now(), now)),
        })
    }

    pub fn reference_hz(&self) -> u64 {
        self.reference_hz
    }

    /// The clock now, read at most every [`FRESH`].
    pub fn current_hz(&self) -> u64 {
        let mut last = self.last.lock().unwrap();
        if last.0.elapsed() >= FRESH
            && let Some(hz) = read_hz(&self.input)
        {
            *last = (Instant::now(), hz);
        }
        last.1
    }

    /// How much work a nanosecond of engine time is now, in nanoseconds at the
    /// reference clock.
    pub fn work_per_ns(&self) -> f64 {
        self.current_hz() as f64 / self.reference_hz as f64
    }
}

fn read_hz(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The last level of `pp_dpm_sclk`, in Hz.
///
/// The last and not the largest: amdgpu inserts the current clock as a level of
/// its own when it is between two, marked `*`, and a boosting card's current
/// clock is above the last level.
fn reference_hz(pp_dpm_sclk: &str) -> Option<u64> {
    let line = pp_dpm_sclk.lines().rfind(|l| !l.trim().is_empty())?;
    let (_, value) = line.split_once(':')?;
    let value = value
        .trim()
        .trim_end_matches('*')
        .trim()
        .to_ascii_lowercase();
    let mhz: u64 = value.strip_suffix("mhz")?.trim().parse().ok()?;
    (mhz > 0).then_some(mhz * 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RDNA4 under load: the current clock is above the reported top level.
    #[test]
    fn the_reference_is_the_last_level_not_the_largest() {
        let table = "0: 500Mhz \n1: 3397Mhz *\n2: 2620Mhz \n";
        assert_eq!(reference_hz(table), Some(2_620_000_000));
    }

    #[test]
    fn a_table_ending_on_the_current_level_still_reads() {
        let table = "0: 500Mhz\n1: 1800Mhz\n2: 2400Mhz *\n";
        assert_eq!(reference_hz(table), Some(2_400_000_000));
    }

    #[test]
    fn nothing_readable_is_no_reference() {
        assert_eq!(reference_hz(""), None);
        assert_eq!(reference_hz("0: fast\n"), None);
        assert_eq!(reference_hz("0: 0Mhz\n"), None);
    }
}
