//! Does the virglrenderer we actually loaded enforce a per-guest VRAM budget?
//!
//! `vram-limit-mib` is not enforced by this process. It is enforced inside
//! virglrenderer, by `patches/0002-virglrenderer-amdgpu-per-guest-VRAM-budget.patch`,
//! which reads `NESTRI_VRAM_LIMIT_MIB` from the environment — the VMM sets the
//! variable and counts allocations, but the refusal happens in the renderer,
//! because that is the only place with a channel that can tell a guest it was
//! refused (see `virtio-devices/src/gpu/vram.rs`).
//!
//! Which renderer gets loaded is decided by `LD_LIBRARY_PATH`, outside this
//! program. Nothing used to check. Point the loader at a stock virglrenderer and
//! the limit silently becomes a no-op: the config still names a number, the
//! stats socket still reports `vram_limit_bytes`, `vram_refusals` sits at zero
//! because nothing is refusing anything, and the first sign of trouble is one
//! guest exhausting a card its neighbours were sharing.
//!
//! **A limit that silently does not apply is worse than no limit**, because it
//! is a limit you have stopped thinking about. So when the config asks for one,
//! nesbox now refuses to start unless it can see the enforcing renderer.
//!
//! # What the check actually proves, and what it does not
//!
//! It finds the `libvirglrenderer` the dynamic loader mapped into this process,
//! reads it, and looks for the string `NESTRI_VRAM_LIMIT_MIB`. Only the patched
//! build reads that variable, so only the patched build contains the name of it.
//!
//! That is evidence, not proof. It shows the loaded library was built from a
//! source tree that knows the variable; it cannot show the enforcement path is
//! reached or correct. A marker *symbol* exported by the patch and resolved with
//! `dlsym` would be a better signal and the patch should grow one — this works
//! against the builds that exist today without rebuilding them.
//!
//! It is deliberately not a substitute for the counters. `vram.rs` totals what
//! the guest asked for and the renderer keeps its own; if the two disagree, one
//! of them is wrong, and that cross-check stays the real answer.

use std::io;
use std::path::{Path, PathBuf};

/// The name the patched renderer reads. Present in that build and no other.
const BUDGET_MARKER: &[u8] = b"NESTRI_VRAM_LIMIT_MIB";

/// What we could determine about the loaded renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Budget {
    /// The loaded library carries the marker.
    Enforced(PathBuf),
    /// It was found and read, and does not carry the marker.
    NotEnforced(PathBuf),
    /// The question could not be answered — no mapping, or it would not read.
    /// Reported separately from "no" so a config error and a missing `/proc`
    /// are not the same message.
    Unknown(String),
}

impl std::fmt::Display for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Budget::Enforced(p) => write!(f, "enforcing ({})", p.display()),
            Budget::NotEnforced(p) => write!(f, "not enforcing ({})", p.display()),
            Budget::Unknown(why) => write!(f, "unknown ({why})"),
        }
    }
}

/// Look for the loaded renderer and say whether it enforces a budget.
pub fn vram_budget() -> Budget {
    let Some(path) = loaded_renderer() else {
        return Budget::Unknown("no libvirglrenderer mapping in /proc/self/maps".into());
    };
    match carries_marker(&path) {
        Ok(true) => Budget::Enforced(path),
        Ok(false) => Budget::NotEnforced(path),
        Err(e) => Budget::Unknown(format!("could not read {}: {e}", path.display())),
    }
}

/// The `libvirglrenderer` this process actually mapped.
///
/// `/proc/self/maps` rather than the link-time name, because the whole failure
/// this guards against is `LD_LIBRARY_PATH` pointing somewhere else: the name
/// the binary was linked against is exactly the answer that would mislead.
fn loaded_renderer() -> Option<PathBuf> {
    find_renderer(&std::fs::read_to_string("/proc/self/maps").ok()?)
}

/// The parsing, with its input passed in so it can be tested.
///
/// It has to be: the test binary for this crate does not link virglrenderer at
/// all — nothing in the library references a rutabaga symbol, so `--as-needed`
/// drops it, even though the `nesbox` binary maps it. Asserting against whatever
/// the test process happens to have loaded tests the harness, not the parser.
fn find_renderer(maps: &str) -> Option<PathBuf> {
    maps.lines()
        // A maps line is `addr perms offset dev inode path`; the path is
        // whatever follows, and is absent for anonymous mappings.
        .filter_map(|line| line.split_whitespace().nth(5))
        .find(|path| {
            Path::new(path)
                .file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.starts_with("libvirglrenderer.so"))
        })
        .map(PathBuf::from)
}

fn carries_marker(path: &Path) -> io::Result<bool> {
    let bytes = std::fs::read(path)?;
    Ok(bytes
        .windows(BUDGET_MARKER.len())
        .any(|w| w == BUDGET_MARKER))
}

/// Check the renderer against what the config asked for.
///
/// A configured limit with no enforcing renderer is a hard error: starting would
/// mean running a guest that believes it is bounded and is not. Everything else
/// is a log line — without a configured limit there is nothing to enforce, and
/// on `Unknown` refusing to start would turn an unreadable `/proc` into an
/// outage over a question that does not apply.
pub fn check(vram_limit_mib: Option<u64>) -> anyhow::Result<()> {
    decide(vram_budget(), vram_limit_mib)
}

/// The decision `check` makes, with the answer supplied rather than probed.
///
/// Split out so the policy is testable without controlling which renderer the
/// test binary happens to have loaded — and kept as the only copy of it, since
/// a policy written twice is a policy that will disagree with itself.
fn decide(budget: Budget, vram_limit_mib: Option<u64>) -> anyhow::Result<()> {
    match (&budget, vram_limit_mib) {
        (Budget::Enforced(p), Some(mib)) => {
            log::info!(
                "gpu: VRAM budget of {mib} MiB will be enforced by {}",
                p.display()
            );
        }
        (Budget::NotEnforced(p), Some(mib)) => {
            anyhow::bail!(
                "config asks for a {mib} MiB VRAM limit, but the loaded renderer does not \
                 enforce one.\n  loaded: {}\n\
                 That limit would silently do nothing: the guest would be told the card's \
                 full size and could take all of it.\n\
                 Either point LD_LIBRARY_PATH at a virglrenderer built with \
                 patches/0002-virglrenderer-amdgpu-per-guest-VRAM-budget.patch, or remove \
                 vram-limit-mib from the config so the absence of a bound is deliberate.",
                p.display()
            );
        }
        (Budget::Unknown(why), Some(mib)) => {
            log::warn!(
                "gpu: config asks for a {mib} MiB VRAM limit and whether the renderer \
                 enforces it could not be determined ({why}). Treat the limit as unproven."
            );
        }
        (_, None) => {
            log::debug!("gpu: no VRAM limit configured; renderer is {budget}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `/proc/self/maps` lines, including the ones that break naive
    /// parsing: an anonymous mapping with no path at all, `[heap]`, and a
    /// same-directory neighbour whose name merely starts the same way.
    const MAPS: &str = "\
55a1c0000000-55a1c0002000 r--p 00000000 fe:02 1234567    /usr/bin/nesbox
7f21f4f41000-7f21f5000000 r-xp 00000000 fe:02 7654321    /home/w/artifacts/virgl-nvalid/lib/libvirglrenderer.so.1.11.0
7f21f5000000-7f21f5001000 rw-p 00000000 00:00 0
7f21f5100000-7f21f5200000 r-xp 00000000 fe:02 7654322    /usr/lib/libvirglrenderer-helper.so.1
7ffd0a1b2000-7ffd0a1d3000 rw-p 00000000 00:00 0          [stack]";

    /// The check has to look at what is *mapped*, not what was linked — that is
    /// the entire failure it guards against.
    #[test]
    fn the_mapped_renderer_is_picked_out_of_a_maps_file() {
        let found = find_renderer(MAPS).expect("the mapping is there");
        assert_eq!(
            found,
            PathBuf::from("/home/w/artifacts/virgl-nvalid/lib/libvirglrenderer.so.1.11.0"),
            "must pick the real library, from wherever LD_LIBRARY_PATH found it"
        );
    }

    #[test]
    fn a_process_with_no_renderer_mapped_yields_nothing() {
        let without = MAPS
            .lines()
            .filter(|l| !l.contains("libvirglrenderer.so.1.11.0"))
            .collect::<Vec<_>>()
            .join("\n");
        // The `-helper` line is still in there, and must not be mistaken for it.
        assert!(without.contains("libvirglrenderer-helper"));
        assert_eq!(find_renderer(&without), None);
    }

    /// And against this process, whatever it happens to be: the parser must not
    /// panic or misread real kernel output, even when the answer is `None`.
    #[test]
    fn real_maps_output_parses_without_panicking() {
        let maps = std::fs::read_to_string("/proc/self/maps").expect("/proc/self/maps");
        if let Some(p) = find_renderer(&maps) {
            assert!(p.is_absolute(), "{p:?}");
        }
    }

    #[test]
    fn a_file_without_the_marker_reads_as_not_enforcing() {
        let dir = std::env::temp_dir().join(format!("nesbox-rend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stock = dir.join("stock.so");
        std::fs::write(&stock, b"nothing interesting in here").unwrap();
        assert!(!carries_marker(&stock).unwrap());

        let patched = dir.join("patched.so");
        let mut blob = b"padding".to_vec();
        blob.extend_from_slice(BUDGET_MARKER);
        blob.extend_from_slice(b"more padding");
        std::fs::write(&patched, &blob).unwrap();
        assert!(carries_marker(&patched).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The case that matters: a limit in the config and nothing enforcing it
    /// must stop the VM, not warn about it. A warning in a log nobody reads is
    /// how a limit stays imaginary for a month.
    #[test]
    fn a_configured_limit_with_no_enforcement_is_fatal() {
        // Exercised through the same match `check` uses, with the outcome
        // supplied rather than probed, so this does not depend on which
        // renderer happens to be loaded while the tests run.
        let err = decide(
            Budget::NotEnforced(PathBuf::from("/usr/lib/libvirglrenderer.so.1")),
            Some(512),
        )
        .expect_err("a limit with no enforcement must be fatal");
        let msg = format!("{err}");
        assert!(msg.contains("512"), "{msg}");
        assert!(msg.contains("vram-limit-mib"), "{msg}");
        assert!(msg.contains("LD_LIBRARY_PATH"), "{msg}");
    }

    #[test]
    fn no_configured_limit_is_never_fatal() {
        for budget in [
            Budget::NotEnforced(PathBuf::from("/x")),
            Budget::Unknown("no /proc".into()),
            Budget::Enforced(PathBuf::from("/y")),
        ] {
            assert!(decide(budget, None).is_ok());
        }
    }

    /// An unanswerable question is not the same as a "no". An unreadable
    /// `/proc` should not stop a box that asked for a limit -- it should say so.
    #[test]
    fn an_unknown_answer_warns_rather_than_refusing() {
        assert!(decide(Budget::Unknown("no /proc".into()), Some(512)).is_ok());
    }
}
