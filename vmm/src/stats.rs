//! The machine-readable surface a supervisor reads instead of the log.
//!
//! One VMM process is one guest, so this describes one box. A supervisor
//! connects, reads one JSON object, and the connection closes.
//!
//! # Why a socket, and why snapshot-on-connect
//!
//! Pull rather than push, because the thing that wants these numbers heartbeats
//! on a timer, and a pull cannot fall behind or need a queue. Snapshot on
//! connect rather than a file refreshed on a timer, because a file is either
//! stale or costs a timer that runs whether anyone is reading; a socket costs
//! nothing until someone asks, and what it returns is true at the moment it is
//! asked.
//!
//! Events -- an eviction, a missed deadline -- do not fit this shape and will
//! want a push channel. That is deliberately not built here: one mechanism for
//! polled state, and a second for events when there are events worth pushing.
//!
//! # Shape of the contract
//!
//! Counters are **raw and monotonic**. Rates and percentages are the reader's
//! job, because computing them here means choosing a window, and the right
//! window depends on a question this process cannot see. `gfx_ns` and `fences`
//! from two snapshots and the wall time between them give occupancy and
//! presentation rate; one snapshot alone gives neither, and that is honest.
//!
//! Absent is not zero. `gpu` is `null` before the guest creates its first GPU
//! context, and `occupancy` is `null` until the DRM client exists. Zeroes would
//! read as an idle GPU, which is a different claim.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use virtio_devices::{GpuDevice, GpuSnapshot};

/// Everything the socket can report. Held by the serving thread.
pub struct StatsSource {
    started: Instant,
    gpu: Option<Arc<GpuDevice>>,
}

impl StatsSource {
    pub fn new(gpu: Option<Arc<GpuDevice>>) -> Self {
        Self {
            started: Instant::now(),
            gpu,
        }
    }

    fn snapshot_json(&self) -> String {
        let uptime_ms = self.started.elapsed().as_millis();
        let gpu = self.gpu.as_ref().map(|g| gpu_json(&g.metrics()));
        format!(
            "{{\"schema\":1,\"uptime_ms\":{uptime_ms},\"gpu\":{}}}\n",
            gpu.unwrap_or_else(|| "null".into())
        )
    }
}

fn gpu_json(s: &GpuSnapshot) -> String {
    let occupancy = match s.occupancy {
        Some(o) => format!(
            "{{\"gfx_ns\":{},\"requested_vram_bytes\":{},\
             \"resident_vram_bytes\":{},\"evicted_vram_bytes\":{}}}",
            o.gfx_ns, o.requested_vram_bytes, o.resident_vram_bytes, o.evicted_vram_bytes
        ),
        None => "null".into(),
    };
    format!(
        "{{\"submits\":{},\"submits_failed\":{},\"fences\":{},\
         \"vram_bytes\":{},\"vram_peak_bytes\":{},\"vram_limit_bytes\":{},\
         \"vram_refusals\":{},\"gtt_bytes\":{},\
         \"window_bytes\":{},\"window_peak_bytes\":{},\"window_limit_bytes\":{},\
         \"window_mappings\":{},\"window_refusals\":{},\
         \"occupancy\":{occupancy}}}",
        s.submits,
        s.submits_failed,
        s.fences,
        s.vram_bytes,
        s.vram_peak_bytes,
        s.vram_limit_bytes,
        s.vram_refusals,
        s.gtt_bytes,
        s.window_bytes,
        s.window_peak_bytes,
        s.window_limit_bytes,
        s.window_mappings,
        s.window_refusals,
    )
}

/// Start serving snapshots on `path`.
///
/// A stale socket from a crashed process is removed first: the alternative is a
/// VMM that refuses to start because a previous one did not clean up, which
/// turns a cosmetic problem into an outage.
pub fn serve(path: PathBuf, source: StatsSource) -> Result<()> {
    if path.exists() {
        // Only if nothing is listening. Removing a live socket would steal the
        // surface from a running VMM.
        if UnixStream::connect(&path).is_ok() {
            anyhow::bail!("another process is already serving stats on {path:?}");
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to clear a stale stats socket at {path:?}"))?;
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to bind the stats socket at {path:?}"))?;
    restrict(&path)?;

    log::info!("stats: serving on {path:?}");

    std::thread::Builder::new()
        .name("nesbox-stats".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(mut s) => {
                        let body = source.snapshot_json();
                        // A reader that hung up mid-write is ordinary, not an
                        // error worth logging at every poll interval.
                        let _ = s.write_all(body.as_bytes());
                    }
                    Err(e) => log::warn!("stats: accept failed: {e}"),
                }
            }
        })
        .context("failed to start the stats thread")?;

    Ok(())
}

/// Owner-only. These numbers describe a tenant's workload, so they are not
/// world-readable by default even on a single-tenant host.
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to restrict {path:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vm_with_no_gpu_reports_null_rather_than_zeroes() {
        let json = StatsSource::new(None).snapshot_json();
        assert!(json.contains("\"gpu\":null"), "{json}");
        assert!(json.contains("\"schema\":1"));
        assert!(
            json.ends_with('\n'),
            "a reader should be able to read a line"
        );
    }

    #[test]
    fn a_gpu_with_no_drm_client_yet_reports_null_occupancy() {
        let s = GpuSnapshot {
            submits: 7,
            fences: 5,
            vram_limit_bytes: 512 << 20,
            ..Default::default()
        };
        let json = gpu_json(&s);
        assert!(json.contains("\"occupancy\":null"), "{json}");
        assert!(json.contains("\"submits\":7"));
        assert!(json.contains("\"vram_limit_bytes\":536870912"));
    }

    #[test]
    fn the_snapshot_is_valid_json() {
        // The format! calls are hand-rolled, so this asserts the thing that
        // hand-rolling gets wrong.
        let s = GpuSnapshot {
            submits: 1,
            submits_failed: 2,
            fences: 3,
            vram_bytes: 4,
            vram_peak_bytes: 5,
            vram_limit_bytes: 6,
            vram_refusals: 7,
            gtt_bytes: 8,
            window_bytes: 20,
            window_peak_bytes: 21,
            window_limit_bytes: 22,
            window_mappings: 23,
            window_refusals: 24,
            occupancy: Some(virtio_devices::Occupancy {
                gfx_ns: 9,
                requested_vram_bytes: 10,
                resident_vram_bytes: 11,
                evicted_vram_bytes: 12,
            }),
        };
        let body = format!("{{\"schema\":1,\"uptime_ms\":1,\"gpu\":{}}}", gpu_json(&s));
        let v: serde_json::Value = serde_json::from_str(&body).expect("must be valid JSON");
        assert_eq!(v["gpu"]["occupancy"]["gfx_ns"], 9);
        assert_eq!(v["gpu"]["submits_failed"], 2);
        assert_eq!(v["gpu"]["gtt_bytes"], 8);
        assert_eq!(v["gpu"]["window_mappings"], 23);
        assert_eq!(v["gpu"]["window_peak_bytes"], 21);
    }
}
