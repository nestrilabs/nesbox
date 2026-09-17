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
use virtio_devices::{CommandKindCounts, GPU_COMMAND_NAMES, GpuDevice, GpuSnapshot, PhaseSnapshot};

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

/// A phase as `{"ns":…,"count":…}`.
///
/// Both halves, never a mean: a mean computed here would be a mean since boot.
/// Two snapshots and the difference of each give the mean over the window the
/// reader actually cares about, which is the same contract `gfx_ns` has.
fn phase_json(p: &PhaseSnapshot) -> String {
    format!("{{\"ns\":{},\"count\":{}}}", p.ns, p.count)
}

/// The per-opcode counts as a JSON array, indexed by opcode number.
///
/// An array and not an object with names: only opcode 2 is identified in this
/// tree, and labelling the rest would publish a guess. The index *is* the
/// opcode, which is the fact we actually have.
fn ccmd_json(counts: &[u64]) -> String {
    let body: Vec<String> = counts.iter().map(|c| c.to_string()).collect();
    format!("[{}]", body.join(","))
}

/// Per-command-kind phases, as an object keyed by the kind's name.
///
/// Named rather than positional, unlike `ccmd` and `info_query`: those index
/// protocols whose numbering is the fact, while this indexes an enum that is
/// ours and whose order carries no meaning outside this binary. A reader
/// should not have to hold that order in their head.
///
/// Kinds that never happened are left out. A zero row is noise on a surface
/// with twenty-six of them, and absent already means zero here.
fn kinds_json(kinds: &CommandKindCounts) -> String {
    let body: Vec<String> = kinds
        .0
        .iter()
        .enumerate()
        .filter(|(_, p)| p.count != 0)
        .map(|(i, p)| format!("\"{}\":{}", GPU_COMMAND_NAMES[i], phase_json(p)))
        .collect();
    format!("{{{}}}", body.join(","))
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
         \"window_mappings\":{},\"window_refusals\":{},\"drained\":{},\
         \"spin\":{spin},\"sleep\":{sleep},\"drain\":{drain},\
         \"command\":{command},\"submit\":{submit},\"observe\":{observe},\
         \"fence_create\":{fence_create},\"fence_latency\":{fence_latency},\
         \"complete\":{complete},\
         \"ccmd\":{ccmd},\"ccmd_high\":{},\"ccmd_records\":{},\
         \"ccmd_malformed\":{},\
         \"info_query\":{info_query},\"info_high\":{},\
         \"rutabaga_map\":{rutabaga_map},\"rutabaga_unmap\":{rutabaga_unmap},\
         \"kvm_map\":{kvm_map},\"kvm_unmap\":{kvm_unmap},\
         \"placed_map\":{placed_map},\"placed_withdraw\":{placed_withdraw},\
         \"place_refused\":{},\
         \"command_kind\":{command_kind},\
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
        s.drained,
        s.ccmd_high,
        s.ccmd_records,
        s.ccmd_malformed,
        s.info_high,
        s.place_refused,
        ccmd = ccmd_json(&s.ccmd),
        info_query = ccmd_json(&s.info_query.0),
        rutabaga_map = phase_json(&s.rutabaga_map),
        rutabaga_unmap = phase_json(&s.rutabaga_unmap),
        kvm_map = phase_json(&s.kvm_map),
        kvm_unmap = phase_json(&s.kvm_unmap),
        placed_map = phase_json(&s.placed_map),
        placed_withdraw = phase_json(&s.placed_withdraw),
        command_kind = kinds_json(&s.command_kind),
        spin = phase_json(&s.spin),
        sleep = phase_json(&s.sleep),
        drain = phase_json(&s.drain),
        command = phase_json(&s.command),
        submit = phase_json(&s.submit),
        observe = phase_json(&s.observe),
        fence_create = phase_json(&s.fence_create),
        fence_latency = phase_json(&s.fence_latency),
        complete = phase_json(&s.complete),
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
            drained: 25,
            spin: PhaseSnapshot { ns: 30, count: 31 },
            sleep: PhaseSnapshot { ns: 32, count: 33 },
            drain: PhaseSnapshot { ns: 34, count: 35 },
            command: PhaseSnapshot { ns: 36, count: 37 },
            submit: PhaseSnapshot { ns: 38, count: 39 },
            observe: PhaseSnapshot { ns: 40, count: 41 },
            fence_create: PhaseSnapshot { ns: 42, count: 43 },
            fence_latency: PhaseSnapshot { ns: 44, count: 45 },
            complete: PhaseSnapshot { ns: 46, count: 47 },
            ccmd: {
                let mut a = [0u64; 16];
                a[2] = 48;
                a[4] = 49;
                a
            },
            ccmd_high: 50,
            ccmd_records: 51,
            ccmd_malformed: 52,
            info_query: {
                let mut a = virtio_devices::InfoCounts::default();
                a.0[0x0e] = 53;
                a
            },
            info_high: 54,
            command_kind: {
                let mut k = CommandKindCounts::default();
                // cmd_submit_3d and resource_map_blob, the two the reader cares
                // about most.
                k.0[19] = PhaseSnapshot { ns: 55, count: 56 };
                k.0[21] = PhaseSnapshot { ns: 57, count: 58 };
                k
            },
            rutabaga_map: PhaseSnapshot { ns: 59, count: 60 },
            rutabaga_unmap: PhaseSnapshot { ns: 61, count: 62 },
            kvm_map: PhaseSnapshot { ns: 63, count: 64 },
            kvm_unmap: PhaseSnapshot { ns: 65, count: 66 },
            placed_map: PhaseSnapshot { ns: 67, count: 68 },
            placed_withdraw: PhaseSnapshot { ns: 69, count: 70 },
            place_refused: 71,
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
        // Every phase carries both halves. A phase that lost its count would
        // still be valid JSON and would silently stop being a mean.
        for phase in [
            "spin",
            "sleep",
            "drain",
            "command",
            "submit",
            "observe",
            "fence_create",
            "fence_latency",
            "complete",
        ] {
            assert!(
                v["gpu"][phase]["ns"].is_number() && v["gpu"][phase]["count"].is_number(),
                "{phase} is not a phase: {}",
                v["gpu"][phase]
            );
        }
        assert_eq!(v["gpu"]["submit"]["ns"], 38);
        assert_eq!(v["gpu"]["submit"]["count"], 39);
        assert_eq!(v["gpu"]["drained"], 25);
        // The opcode array is indexed by opcode, so its position carries
        // meaning that a reordering or a truncation would silently destroy.
        assert_eq!(v["gpu"]["ccmd"].as_array().map(Vec::len), Some(16));
        assert_eq!(v["gpu"]["ccmd"][2], 48);
        assert_eq!(v["gpu"]["ccmd"][4], 49);
        assert_eq!(v["gpu"]["ccmd_records"], 51);
        assert_eq!(v["gpu"]["info_query"].as_array().map(Vec::len), Some(64));
        assert_eq!(v["gpu"]["info_query"][0x0e], 53);
        assert_eq!(v["gpu"]["info_high"], 54);
        assert_eq!(v["gpu"]["kvm_map"]["ns"], 63);
        assert_eq!(v["gpu"]["placed_map"]["ns"], 67);
        assert_eq!(v["gpu"]["place_refused"], 71);

        // Named, not positional: the enum's order is ours and means nothing to
        // a reader, so the kind is carried by its name.
        assert_eq!(v["gpu"]["command_kind"]["cmd_submit_3d"]["ns"], 55);
        assert_eq!(v["gpu"]["command_kind"]["cmd_submit_3d"]["count"], 56);
        assert_eq!(v["gpu"]["command_kind"]["resource_map_blob"]["count"], 58);
        // A kind that never happened is absent rather than a zero row: with
        // twenty-six of them the zeroes would be most of the output.
        assert!(v["gpu"]["command_kind"]["get_edid"].is_null());
        assert_eq!(v["gpu"]["submits_failed"], 2);
        assert_eq!(v["gpu"]["gtt_bytes"], 8);
        assert_eq!(v["gpu"]["window_mappings"], 23);
        assert_eq!(v["gpu"]["window_peak_bytes"], 21);
    }
}
