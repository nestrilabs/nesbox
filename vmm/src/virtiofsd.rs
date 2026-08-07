//! Supervision of virtiofsd, the vhost-user daemon behind each virtio-fs device.
//!
//! One daemon per shared directory. The VMM owns them: they are spawned before
//! the device connects and killed when the VMM drops, so a shared directory can
//! never outlive the VM that mounted it.

use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Directories that ship virtiofsd outside `PATH`.
///
/// It is a vhost-user backend rather than a user-facing tool, so several
/// distributions deliberately keep it out of `PATH`: Arch installs
/// `/usr/lib/virtiofsd`, Fedora and Debian use `/usr/libexec`. A plain
/// `Command::new("virtiofsd")` therefore fails on a machine where virtiofsd is
/// perfectly well installed, which is a confusing thing to be told.
const FALLBACK_DIRS: &[&str] = &[
    "/usr/lib",
    "/usr/libexec",
    "/usr/local/lib",
    "/usr/local/libexec",
    "/usr/lib/qemu",
];

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Find virtiofsd, or say where it was looked for.
///
/// `NESBOX_VIRTIOFSD` wins outright, so a machine that keeps it somewhere
/// unusual needs no code change. Otherwise `PATH` first, then the directories
/// distributions actually use.
pub fn binary_path() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("NESBOX_VIRTIOFSD") {
        let path = PathBuf::from(&explicit);
        anyhow::ensure!(
            is_executable(&path),
            "NESBOX_VIRTIOFSD is set to {}, which is not an executable file",
            path.display()
        );
        return Ok(path);
    }
    resolve(std::env::var_os("PATH"), FALLBACK_DIRS)
}

/// The search itself, with its inputs passed in so it can be tested.
fn resolve(path_env: Option<OsString>, fallbacks: &[&str]) -> Result<PathBuf> {
    let mut searched: Vec<PathBuf> = Vec::new();
    let from_path = path_env
        .as_ref()
        .map(|p| std::env::split_paths(p).collect::<Vec<_>>())
        .unwrap_or_default();

    for dir in from_path.iter().map(PathBuf::as_path).chain(
        fallbacks
            .iter()
            .map(Path::new)
            // A fallback that is already on PATH would otherwise be reported
            // twice in the error.
            .filter(|d| !from_path.iter().any(|p| p == *d)),
    ) {
        let candidate = dir.join("virtiofsd");
        if is_executable(&candidate) {
            return Ok(candidate);
        }
        searched.push(candidate);
    }

    anyhow::bail!(
        "could not find virtiofsd. Set NESBOX_VIRTIOFSD to its path, or install it. Looked in: {}",
        searched
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// A running virtiofsd, killed on drop.
pub struct Virtiofsd {
    child: Child,
    socket_path: PathBuf,
    tag: String,
}

impl Virtiofsd {
    /// Spawn a daemon serving `shared_dir` and wait for its socket to appear.
    ///
    /// `sandbox=none` is used because the VMM is expected to run unprivileged:
    /// virtiofsd's namespace sandbox needs privileges we do not have, and the
    /// isolation that matters here comes from the VM boundary.
    pub fn spawn(
        tag: &str,
        shared_dir: &Path,
        read_only: bool,
        runtime_dir: &Path,
    ) -> Result<Self> {
        anyhow::ensure!(
            shared_dir.is_dir(),
            "shared directory {} does not exist",
            shared_dir.display()
        );
        std::fs::create_dir_all(runtime_dir).with_context(|| {
            format!(
                "failed to create runtime directory {}",
                runtime_dir.display()
            )
        })?;
        let socket_path = runtime_dir.join(format!("virtiofs-{tag}.sock"));
        let _ = std::fs::remove_file(&socket_path);

        let binary = binary_path()?;
        log::debug!("using virtiofsd at {}", binary.display());
        let mut cmd = Command::new(&binary);
        cmd.arg("--socket-path")
            .arg(&socket_path)
            .arg("--shared-dir")
            .arg(shared_dir)
            .arg("--sandbox")
            .arg("none")
            .arg("--cache")
            .arg("auto")
            // Follow our own verbosity, so tracing the VMM traces the FUSE
            // traffic too.
            .arg("--log-level")
            .arg(if log::log_enabled!(log::Level::Debug) {
                "debug"
            } else {
                "warn"
            })
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        if read_only {
            cmd.arg("--readonly");
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn virtiofsd at {}", binary.display()))?;

        let mut daemon = Self {
            child,
            socket_path,
            tag: tag.to_string(),
        };
        daemon.wait_for_socket()?;
        log::info!(
            "virtiofsd serving {} as \"{}\"{}",
            shared_dir.display(),
            tag,
            if read_only { " (read-only)" } else { "" }
        );
        Ok(daemon)
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Poll for the listening socket, failing fast if the daemon exits.
    fn wait_for_socket(&mut self) -> Result<()> {
        for _ in 0..100 {
            if self.socket_path.exists() {
                return Ok(());
            }
            if let Some(status) = self
                .child
                .try_wait()
                .context("failed to check on virtiofsd")?
            {
                anyhow::bail!("virtiofsd for \"{}\" exited early: {status}", self.tag);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        anyhow::bail!(
            "virtiofsd for \"{}\" did not create {} within 5s",
            self.tag,
            self.socket_path.display()
        )
    }
}

impl Drop for Virtiofsd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory holding a fake, executable `virtiofsd`.
    fn with_binary(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nesbox-vfsd-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("virtiofsd");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[test]
    fn a_binary_outside_path_is_still_found() {
        // The case that made this necessary: Arch installs /usr/lib/virtiofsd,
        // which is not on PATH, and Command::new("virtiofsd") then reports it
        // as not installed.
        let dir = with_binary("fallback");
        let found = resolve(
            Some(OsString::from("/nonexistent")),
            &[dir.to_str().unwrap()],
        )
        .expect("a fallback directory must be searched");
        assert_eq!(found, dir.join("virtiofsd"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_wins_over_the_fallbacks() {
        // A distribution that does put it on PATH should not be overridden by
        // a stale copy somewhere else.
        let on_path = with_binary("onpath");
        let fallback = with_binary("notpath");
        let found = resolve(
            Some(OsString::from(on_path.to_str().unwrap())),
            &[fallback.to_str().unwrap()],
        )
        .expect("found");
        assert_eq!(found, on_path.join("virtiofsd"));
        let _ = std::fs::remove_dir_all(&on_path);
        let _ = std::fs::remove_dir_all(&fallback);
    }

    #[test]
    fn a_non_executable_file_does_not_count() {
        // A stray text file named virtiofsd would otherwise be "found" and then
        // fail to spawn, reporting the wrong problem.
        let dir = std::env::temp_dir().join(format!("nesbox-vfsd-noexec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("virtiofsd"), b"not a program").unwrap();
        assert!(resolve(None, &[dir.to_str().unwrap()]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failure_names_everywhere_it_looked() {
        // "is it installed?" is not actionable when the answer is yes and it is
        // simply somewhere else.
        let err = resolve(Some(OsString::from("/nowhere-a")), &["/nowhere-b"])
            .expect_err("nothing to find");
        let message = format!("{err}");
        assert!(message.contains("/nowhere-a/virtiofsd"), "{message}");
        assert!(message.contains("/nowhere-b/virtiofsd"), "{message}");
        assert!(message.contains("NESBOX_VIRTIOFSD"), "{message}");
    }
}
