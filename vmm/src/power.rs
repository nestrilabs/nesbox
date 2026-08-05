//! ACPI sleep and reset registers.
//!
//! Under hardware-reduced ACPI there is no PM1 control block: a guest powers
//! off by writing SLP_TYP/SLP_EN to the Sleep Control register named in the
//! FADT, and reboots by writing the reset value to the Reset register. Both
//! are plain I/O ports here, and both end the VM.
//!
//! Without these a guest cannot stop itself at all — Linux reports "Power off
//! not available: System halted instead" and the VM hangs forever.

use crate::lifecycle::{ExitReason, Shutdown};
use std::sync::Arc;

/// Sleep Control register.
pub const SLEEP_CONTROL_PORT: u16 = 0x600;
/// Sleep Status register.
pub const SLEEP_STATUS_PORT: u16 = 0x601;
/// Reset register. 0xCF9 is the conventional choice and matches what Linux
/// already tries when rebooting.
pub const RESET_PORT: u16 = 0xcf9;
/// Value the guest writes to the reset register to reboot.
pub const RESET_VALUE: u8 = 0x0e;

/// SLP_EN, bit 5 of the Sleep Control register: perform the transition.
const SLP_EN: u8 = 1 << 5;
/// SLP_TYP, bits 4:2: which sleep state.
const SLP_TYP_MASK: u8 = 0b0001_1100;
const SLP_TYP_SHIFT: u32 = 2;
/// The sleep type meaning "soft off", matching the \_S5 package in the DSDT.
pub const SLP_TYP_S5: u8 = 5;

pub struct PowerDevice {
    shutdown: Arc<Shutdown>,
}

impl PowerDevice {
    pub fn new(shutdown: Arc<Shutdown>) -> Self {
        Self { shutdown }
    }

    pub fn handles(port: u16) -> bool {
        matches!(port, SLEEP_CONTROL_PORT | SLEEP_STATUS_PORT | RESET_PORT)
    }

    pub fn write(&self, port: u16, data: &[u8]) {
        let Some(&value) = data.first() else { return };
        match port {
            SLEEP_CONTROL_PORT => {
                if value & SLP_EN == 0 {
                    return; // arming the register, not entering the state
                }
                let sleep_type = (value & SLP_TYP_MASK) >> SLP_TYP_SHIFT;
                if sleep_type == SLP_TYP_S5 {
                    self.shutdown.request(ExitReason::PowerOff);
                } else {
                    log::warn!("guest asked for unsupported sleep state S{sleep_type}");
                }
            }
            RESET_PORT => {
                // Linux writes here on reboot. Bit 1 is the "do it" bit; the
                // full value also selects warm or cold reset, which we ignore.
                if value & 0x02 != 0 {
                    self.shutdown.request(ExitReason::Reset);
                }
            }
            _ => {}
        }
    }

    pub fn read(&self, port: u16, data: &mut [u8]) {
        // Nothing is ever pending: a sleep transition ends the VM.
        let value = match port {
            SLEEP_STATUS_PORT => 0,
            _ => 0,
        };
        if let Some(b) = data.first_mut() {
            *b = value;
        }
        if data.len() > 1 {
            data[1..].fill(0);
        }
    }
}
