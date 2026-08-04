//! Supervision of virtiofsd, the vhost-user daemon behind each virtio-fs device.
//!
//! One daemon per shared directory. The VMM owns them: they are spawned before
//! the device connects and killed when the VMM drops, so a shared directory can
//! never outlive the VM that mounted it.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

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
    pub fn spawn(tag: &str, shared_dir: &Path, read_only: bool, runtime_dir: &Path) -> Result<Self> {
        anyhow::ensure!(
            shared_dir.is_dir(),
            "shared directory {} does not exist",
            shared_dir.display()
        );
        std::fs::create_dir_all(runtime_dir).with_context(|| {
            format!("failed to create runtime directory {}", runtime_dir.display())
        })?;
        let socket_path = runtime_dir.join(format!("virtiofs-{tag}.sock"));
        let _ = std::fs::remove_file(&socket_path);

        let mut cmd = Command::new("virtiofsd");
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
            .arg(if log::log_enabled!(log::Level::Debug) { "debug" } else { "warn" })
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        if read_only {
            cmd.arg("--readonly");
        }

        let child = cmd
            .spawn()
            .context("failed to spawn virtiofsd — is it installed and on PATH?")?;

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
