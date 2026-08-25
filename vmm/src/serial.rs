//! 16550A UART at the legacy COM1 port range (0x3F8–0x3FF).
//!
//! This device exists for one reason: `earlyprintk=serial,ttyS0` works before
//! PCI enumeration, so it is the only way to see why a guest died during early
//! boot. Transmit is fully emulated (bytes go to stdout); receive is not wired
//! up — interactive input belongs to virtio-console.

use std::io::Write;
use std::sync::Mutex;

pub const COM1_BASE: u16 = 0x3f8;
pub const COM1_LAST: u16 = 0x3ff;

// Register offsets (DLAB=0 unless noted).
const REG_DATA: u16 = 0; // THR (write) / RBR (read); DLL when DLAB=1
const REG_IER: u16 = 1; // interrupt enable; DLM when DLAB=1
const REG_IIR: u16 = 2; // interrupt identification (read) / FCR (write)
const REG_LCR: u16 = 3; // line control
const REG_MCR: u16 = 4; // modem control
const REG_LSR: u16 = 5; // line status
const REG_MSR: u16 = 6; // modem status
const REG_SCR: u16 = 7; // scratch

const LCR_DLAB: u8 = 0x80;
const LSR_DATA_READY: u8 = 0x01;
const LSR_THR_EMPTY: u8 = 0x20;
const LSR_TRANSMITTER_IDLE: u8 = 0x40;
const IIR_NO_INTERRUPT: u8 = 0x01;
const MSR_DEFAULTS: u8 = 0x20 | 0x10 | 0x80; // DSR | CTS | DCD

struct Inner {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    divisor: u16,
    /// Line-buffered so guest output interleaves cleanly with VMM logging.
    out: Vec<u8>,
}

pub struct Serial {
    inner: Mutex<Inner>,
}

impl Default for Serial {
    fn default() -> Self {
        Self::new()
    }
}

impl Serial {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                ier: 0,
                lcr: 0x03, // 8N1
                mcr: 0x08, // OUT2 set, as firmware would leave it
                scr: 0,
                divisor: 12, // 9600 baud at 115200 base; the guest overwrites it
                out: Vec::with_capacity(256),
            }),
        }
    }

    /// Returns true if `port` belongs to this device.
    pub fn handles(port: u16) -> bool {
        (COM1_BASE..=COM1_LAST).contains(&port)
    }

    pub fn write(&self, port: u16, data: &[u8]) {
        let Some(&byte) = data.first() else { return };
        let mut i = self.inner.lock().unwrap();
        let dlab = i.lcr & LCR_DLAB != 0;

        match port - COM1_BASE {
            REG_DATA if dlab => i.divisor = (i.divisor & 0xff00) | byte as u16,
            REG_DATA => {
                i.out.push(byte);
                if byte == b'\n' || i.out.len() >= 256 {
                    flush(&mut i.out);
                }
            }
            REG_IER if dlab => i.divisor = (i.divisor & 0x00ff) | ((byte as u16) << 8),
            REG_IER => i.ier = byte & 0x0f,
            REG_IIR => {} // FCR: FIFO control, nothing to configure
            REG_LCR => i.lcr = byte,
            REG_MCR => i.mcr = byte,
            REG_SCR => i.scr = byte,
            _ => {}
        }
    }

    pub fn read(&self, port: u16, data: &mut [u8]) {
        let mut i = self.inner.lock().unwrap();
        let dlab = i.lcr & LCR_DLAB != 0;

        let val = match port - COM1_BASE {
            REG_DATA if dlab => i.divisor as u8,
            REG_DATA => 0, // no RX path
            REG_IER if dlab => (i.divisor >> 8) as u8,
            REG_IER => i.ier,
            REG_IIR => IIR_NO_INTERRUPT,
            REG_LCR => i.lcr,
            REG_MCR => i.mcr,
            // Always ready to transmit, and `LSR_DATA_READY` is deliberately
            // never set: this port is write-only, guest input arrives on the
            // virtio console. It used to be written as `| (0 & LSR_DATA_READY)`
            // to show that, which is an expression that can only be zero.
            REG_LSR => LSR_THR_EMPTY | LSR_TRANSMITTER_IDLE,
            REG_MSR => MSR_DEFAULTS,
            REG_SCR => i.scr,
            _ => 0,
        };

        // Flushing on LSR polling keeps partial lines visible when the guest
        // dies mid-message — which is the whole point of this device.
        if port - COM1_BASE == REG_LSR {
            flush(&mut i.out);
        }

        if let Some(b) = data.first_mut() {
            *b = val;
        }
        if data.len() > 1 {
            data[1..].fill(0);
        }
    }
}

fn flush(buf: &mut Vec<u8>) {
    if buf.is_empty() {
        return;
    }
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(buf);
    let _ = stdout.flush();
    buf.clear();
}
