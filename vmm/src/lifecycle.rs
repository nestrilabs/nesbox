//! VM lifetime: why a VM stopped, and how to stop it.
//!
//! A launcher supervising a game needs to tell "the guest shut itself down"
//! from "the VM died", so every path that ends the VM records a reason. It
//! also needs stopping a VM to actually stop everything inside it, which for a
//! microVM means destroying it — the process exiting tears down guest memory,
//! the vCPUs and every device backend with it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Why the VM stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// The guest asked to power off. The expected end of a session.
    PowerOff,
    /// The guest asked to reboot. We do not re-launch it.
    Reset,
    /// The guest triple-faulted or KVM reported a shutdown we did not ask for.
    GuestFault,
    /// The host asked us to stop, via SIGTERM or SIGINT.
    HostSignal,
    /// Something in the VMM failed.
    Error(String),
}

impl ExitReason {
    /// Did the VM stop the way it was supposed to?
    pub fn is_clean(&self) -> bool {
        matches!(self, Self::PowerOff | Self::HostSignal)
    }

    /// Process exit code to report to whoever is supervising us.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::PowerOff | Self::HostSignal => 0,
            Self::Reset => 2,
            Self::GuestFault => 3,
            Self::Error(_) => 1,
        }
    }
}

impl std::fmt::Display for ExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PowerOff => write!(f, "guest powered off"),
            Self::Reset => write!(f, "guest requested a reset"),
            Self::GuestFault => write!(f, "guest faulted"),
            Self::HostSignal => write!(f, "stopped by signal"),
            Self::Error(e) => write!(f, "VMM error: {e}"),
        }
    }
}

/// Shared stop signal. The first reason recorded is the one reported, so the
/// cause is not overwritten by the shutdown it triggers.
#[derive(Default)]
pub struct Shutdown {
    requested: AtomicBool,
    reason: Mutex<Option<ExitReason>>,
}

impl Shutdown {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Ask the VM to stop, recording why. Later calls do not displace the
    /// first reason.
    pub fn request(&self, reason: ExitReason) {
        let mut slot = self.reason.lock().unwrap();
        if slot.is_none() {
            log::info!("shutting down: {reason}");
            *slot = Some(reason);
        }
        self.requested.store(true, Ordering::SeqCst);
    }

    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// The recorded reason, if the VM has been asked to stop.
    pub fn reason(&self) -> Option<ExitReason> {
        self.reason.lock().unwrap().clone()
    }
}
