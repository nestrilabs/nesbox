// A ceiling on how much of the card one guest may use.
//
// # Not how guests share the card
//
// Sharing is the kernel's: with the DRM scheduler's FAIR policy
// (`gpu_sched.sched_policy=2`) every client gets an equal share of the card
// whenever it is contended, and the idle rest whenever it is not. That is
// finer than anything here -- it decides per job -- and it uses the whole
// card. Caps from this file were the sharing once, and left a 160 W card
// running at 61-109 W: guests held back left it idle, and it clocked down.
//
// This is only a ceiling, for a guest that must never have more than a set
// amount however idle the card is. It holds back the guest's *next*
// submission while the guest is over it, which is the one place the host can
// slow a guest without cooperation from it. Nothing the guest does above this
// line -- its driver, its frame limiter, its settings -- can undo it.
//
// It limits **work**: engine time, taken from the kernel's own per-client
// counters for the graphics and compute engines, scaled by the shader clock it
// ran at against the clock the card promises. Not submissions or fences: a
// fence measures how long a job took to come back, which includes time queued
// behind other clients; engine time is what this guest actually used. See
// `occupancy.rs`.
//
// Scaled, because time alone is not a share of the card. A quarter of the
// engine's time on a card the limit itself has let drop to a low clock is far
// less than a quarter of the card, and the card drops its clock exactly
// because limited guests leave it idle. See `clock.rs`.
//
// # How
//
// A token bucket in nanoseconds of engine time at the reference clock. It fills
// at `percent` of wall time and drains by what the engine reports the guest
// used, times the clock it is running at over the reference. A submission is
// let through while the bucket is not empty; otherwise the worker sleeps for
// as long as the debt takes to repay at the fill rate, then looks again. The
// bucket is capped, so a guest that was idle cannot bank a long burst.
//
// The limit is an atomic read on every check, so it can be changed while the
// guest runs and takes effect at the next submission.
//
// # What it does not do
//
// It cannot recall work already submitted, so a guest can overshoot by one
// batch and by what the engine ran between two samples. It reads the
// accounting at most every millisecond while under its limit, so a guest well
// inside its share pays one atomic load per submission and nothing else.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use super::occupancy::Occupancy;

/// The value meaning "no limit".
pub const UNLIMITED: u32 = 100;

/// How much engine time a guest may spend in a burst after being idle.
const BURST_NS: f64 = 4_000_000.0;
/// The deepest the debt may get, so one huge batch is not punished for long.
const MAX_DEBT_NS: f64 = 100_000_000.0;
/// How often the accounting is read while the guest is inside its share.
const SAMPLE_OK: Duration = Duration::from_millis(1);
/// How often it is read while the guest is being held back: the sleep is short,
/// so the picture has to be fresh or the worker sleeps on stale news.
const SAMPLE_HELD: Duration = Duration::from_micros(200);
/// The longest single sleep, so a raised limit is noticed promptly.
const MAX_SLEEP: Duration = Duration::from_millis(2);
const MIN_SLEEP: Duration = Duration::from_micros(100);

struct Pace {
    /// When the accounting was last read.
    at: Instant,
    /// The engine-time counters (graphics and compute) at that read, once there
    /// is one.
    engine_ns: Option<u64>,
    /// Engine time the guest may still spend. Negative is debt.
    bank_ns: f64,
}

pub struct GpuBudget {
    percent: AtomicU32,
    pace: Mutex<Pace>,
}

impl GpuBudget {
    pub fn new(percent: Option<u32>) -> Self {
        let b = Self {
            percent: AtomicU32::new(UNLIMITED),
            pace: Mutex::new(Pace {
                at: Instant::now(),
                engine_ns: None,
                bank_ns: BURST_NS,
            }),
        };
        b.set(percent);
        b
    }

    /// Change the limit. `None` or 100 removes it; anything else is clamped to
    /// 1..=99.
    pub fn set(&self, percent: Option<u32>) {
        let p = percent.map_or(UNLIMITED, |p| p.clamp(1, UNLIMITED));
        self.percent.store(p, Ordering::Relaxed);
    }

    /// The limit in force, as a percentage of the graphics engine.
    pub fn percent(&self) -> u32 {
        self.percent.load(Ordering::Relaxed)
    }

    /// Block until the guest is inside its share. Returns how long it waited.
    ///
    /// `read` gives the engine's accounting for this guest; `None` means there
    /// is no DRM client yet, and a guest with no client has used nothing.
    /// `work_per_ns` is how much work a nanosecond of engine time is right now,
    /// as engine time at the reference clock: 1.0 for a card that reports no
    /// clock, which is then charged plain time.
    pub fn pace(
        &self,
        read: impl Fn() -> Option<Occupancy>,
        work_per_ns: impl Fn() -> f64,
        stop: &AtomicBool,
    ) -> Duration {
        let mut waited = Duration::ZERO;
        loop {
            let percent = self.percent.load(Ordering::Relaxed);
            if percent >= UNLIMITED {
                return waited;
            }
            let rate = f64::from(percent) / 100.0;

            let sleep_for = {
                let mut st = self.pace.lock().unwrap();
                let now = Instant::now();
                let due = if st.bank_ns < 0.0 {
                    SAMPLE_HELD
                } else {
                    SAMPLE_OK
                };
                if st.engine_ns.is_none() || now.duration_since(st.at) >= due {
                    let Some(sample) = read() else {
                        return waited;
                    };
                    let engine_ns = sample.gfx_ns.saturating_add(sample.compute_ns);
                    if let Some(prev) = st.engine_ns {
                        let wall = now.duration_since(st.at).as_nanos() as f64;
                        let used = engine_ns.saturating_sub(prev) as f64 * work_per_ns();
                        st.bank_ns =
                            (st.bank_ns + rate * wall - used).clamp(-MAX_DEBT_NS, BURST_NS);
                    }
                    st.engine_ns = Some(engine_ns);
                    st.at = now;
                }
                if st.bank_ns >= 0.0 {
                    return waited;
                }
                Duration::from_nanos((-st.bank_ns / rate) as u64).clamp(MIN_SLEEP, MAX_SLEEP)
            };

            if stop.load(Ordering::Acquire) {
                return waited;
            }
            std::thread::sleep(sleep_for);
            waited += sleep_for;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn sample(ns: u64) -> Option<Occupancy> {
        Some(Occupancy {
            gfx_ns: ns,
            engine_time: true,
            ..Default::default()
        })
    }

    #[test]
    fn unlimited_never_waits_or_reads() {
        let b = GpuBudget::new(None);
        let reads = AtomicU64::new(0);
        let waited = b.pace(
            || {
                reads.fetch_add(1, Ordering::Relaxed);
                sample(0)
            },
            || 1.0,
            &AtomicBool::new(false),
        );
        assert_eq!(waited, Duration::ZERO);
        assert_eq!(reads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn no_drm_client_yet_is_not_a_reason_to_wait() {
        let b = GpuBudget::new(Some(10));
        assert_eq!(
            b.pace(|| None, || 1.0, &AtomicBool::new(false)),
            Duration::ZERO
        );
    }

    #[test]
    fn a_guest_using_more_than_its_share_is_held_back() {
        let b = GpuBudget::new(Some(10));
        let stop = AtomicBool::new(false);
        // First look establishes the baseline and lets the call through.
        b.pace(|| sample(0), || 1.0, &stop);
        // The engine reports 50 ms spent over the next few milliseconds of wall
        // time, far above 10% of it: the guest has to wait.
        std::thread::sleep(Duration::from_millis(2));
        let waited = b.pace(|| sample(50_000_000), || 1.0, &stop);
        assert!(
            waited > Duration::ZERO,
            "an overdrawn guest was let straight through"
        );
    }

    /// The same engine time on a card at half the reference clock is half the
    /// work, and does not hold the guest back where full-clock time would.
    #[test]
    fn engine_time_on_a_slow_clock_is_charged_as_less_work() {
        // 10% of ~10 ms of wall time, plus the 4 ms an idle guest may burst:
        // 7 ms of full-clock engine time is past that, 3.5 ms of work is not.
        let run = |work_per_ns: f64| {
            let b = GpuBudget::new(Some(10));
            let stop = AtomicBool::new(false);
            b.pace(|| sample(0), || work_per_ns, &stop);
            std::thread::sleep(Duration::from_millis(10));
            b.pace(|| sample(7_000_000), || work_per_ns, &stop)
        };
        assert!(run(1.0) > Duration::ZERO, "full-clock time past the share");
        assert_eq!(
            run(0.5),
            Duration::ZERO,
            "the same time at half clock is half the work"
        );
    }

    /// Compute-engine time is the guest's as much as graphics time is.
    #[test]
    fn compute_engine_time_is_charged_too() {
        let b = GpuBudget::new(Some(10));
        let stop = AtomicBool::new(false);
        b.pace(|| sample(0), || 1.0, &stop);
        std::thread::sleep(Duration::from_millis(2));
        let compute_only = Some(Occupancy {
            compute_ns: 50_000_000,
            engine_time: true,
            ..Default::default()
        });
        assert!(b.pace(|| compute_only, || 1.0, &stop) > Duration::ZERO);
    }

    #[test]
    fn the_limit_can_be_changed_while_running() {
        let b = GpuBudget::new(Some(10));
        assert_eq!(b.percent(), 10);
        b.set(Some(250));
        assert_eq!(b.percent(), UNLIMITED);
        b.set(Some(0));
        assert_eq!(b.percent(), 1);
        b.set(None);
        assert_eq!(b.percent(), UNLIMITED);
    }
}
