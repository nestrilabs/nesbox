//! The surface a supervising agent uses to change a running guest's limits.
//!
//! The stats socket only reads. A limit that can only be set at boot has to be
//! guessed at boot, and a guest's real needs show up while it runs, so the
//! limits that can safely move are settable here, one line at a time, and take
//! effect at the next opportunity without a restart.
//!
//! # Protocol
//!
//! One request per connection: a line of text, answered by one line of JSON.
//!
//! ```text
//! gpu-time-percent            -> {"ok":true,"gpu_time_percent":100}
//! gpu-time-percent 60         -> {"ok":true,"gpu_time_percent":60}
//! gpu-time-percent off        -> {"ok":true,"gpu_time_percent":100}
//! ```
//!
//! A value outside 1..=100 is an error and changes nothing, so a typo cannot
//! quietly become some other limit. 100 and `off` both mean no limit.
//!
//! Owner-only, like the stats socket: it changes what a tenant may use.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use virtio_devices::GpuDevice;

/// What a request can change.
pub struct ControlTarget {
    pub gpu: Option<Arc<GpuDevice>>,
}

/// Apply one request line and say what happened. Pure of the socket so it can
/// be tested without one.
pub fn handle(target: &ControlTarget, line: &str) -> String {
    let mut words = line.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some("gpu-time-percent"), value, None) => {
            let Some(gpu) = &target.gpu else {
                return error("this guest has no GPU");
            };
            match value {
                None => {}
                Some("off") => gpu.set_gpu_time_percent(None),
                Some(v) => match v.parse::<u32>() {
                    Ok(p) if (1..=100).contains(&p) => gpu.set_gpu_time_percent(Some(p)),
                    _ => return error("gpu-time-percent takes 1 to 100, or off"),
                },
            }
            format!(
                "{{\"ok\":true,\"gpu_time_percent\":{}}}\n",
                gpu.gpu_time_percent()
            )
        }
        (Some(other), ..) => error(&format!("unknown request {other:?}")),
        (None, ..) => error("empty request"),
    }
}

fn error(why: &str) -> String {
    format!("{{\"ok\":false,\"error\":{}}}\n", serde_json::json!(why))
}

pub fn serve(path: PathBuf, target: ControlTarget) -> Result<()> {
    if path.exists() {
        // Only if nothing is listening: removing a live socket would take the
        // surface away from a running VMM.
        if UnixStream::connect(&path).is_ok() {
            anyhow::bail!("another process is already serving control on {path:?}");
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to clear a stale control socket at {path:?}"))?;
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to bind the control socket at {path:?}"))?;
    restrict(&path)?;
    log::info!("control: serving on {path:?}");

    std::thread::Builder::new()
        .name("nesbox-control".into())
        .spawn(move || {
            virtio_devices::sched::lower_this_thread("control");
            for stream in listener.incoming() {
                match stream {
                    Ok(mut s) => {
                        // A client that connects and says nothing must not hold
                        // the one thread that answers everyone else.
                        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut line = String::new();
                        let reply = match BufReader::new((&s).take(512)).read_line(&mut line) {
                            Ok(_) => handle(&target, &line),
                            Err(_) => error("no request"),
                        };
                        let _ = s.write_all(reply.as_bytes());
                    }
                    Err(e) => log::warn!("control: accept failed: {e}"),
                }
            }
        })
        .context("failed to start the control thread")?;
    Ok(())
}

fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to restrict {path:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guest_without_a_gpu_refuses_rather_than_pretending() {
        let t = ControlTarget { gpu: None };
        let v: serde_json::Value =
            serde_json::from_str(&handle(&t, "gpu-time-percent 50\n")).unwrap();
        assert_eq!(v["ok"], false);
    }

    #[test]
    fn unknown_and_empty_requests_are_errors() {
        let t = ControlTarget { gpu: None };
        for line in ["", "\n", "frobnicate 1\n", "gpu-time-percent 1 2\n"] {
            let v: serde_json::Value = serde_json::from_str(&handle(&t, line)).unwrap();
            assert_eq!(v["ok"], false, "{line:?}");
        }
    }
}
