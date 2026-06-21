// serial.rs
//
// 16550 UART emulation, wired to stdout.
//
// The kernel writes characters to I/O port 0x3F8 (COM1 base). We intercept
// these writes via KVM vmexit and forward the bytes to the host's stdout.
// The kernel also reads status registers (especially LSR at offset 5) to
// check if the transmit buffer is ready; vm-superio handles this state.
//
// Serial input (host stdin → guest) is not wired for the MVP. The kernel
// will not try to read from the serial port before it panics, so there is
// nothing to send. Adding input would require an event loop to poll stdin,
// which is deferred until we have a second I/O source that forces one.

use std::io::{self, Stdout, Write};
use std::ops::Deref;

use vm_superio::Trigger;
use vm_superio::serial::{NoEvents, Serial};
use vmm_sys_util::eventfd::EventFd;

/// Adapts vmm-sys-util's `EventFd` to vm-superio's `Trigger` trait.
///
/// `vm-superio`'s `Serial` requires a `Trigger` it can pulse to raise an
/// interrupt. `EventFd` does not implement `Trigger` directly — both are
/// foreign types, so the orphan rule forbids a direct impl — so we wrap it.
/// This is the canonical rust-vmm pattern (Firecracker carries the same
/// wrapper). For the MVP we never actually pull the trigger, because we
/// don't deliver serial interrupts to the guest; the wrapper exists only to
/// satisfy the type bound.
pub struct EventFdTrigger(EventFd);

impl Trigger for EventFdTrigger {
    type E = io::Error;

    fn trigger(&self) -> io::Result<()> {
        self.0.write(1)
    }
}

impl Deref for EventFdTrigger {
    type Target = EventFd;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl EventFdTrigger {
    /// Create an interrupt `EventFd` with the given `eventfd(2)` flags.
    pub fn new(flag: i32) -> io::Result<Self> {
        Ok(EventFdTrigger(EventFd::new(flag)?))
    }
}

/// The base I/O port for COM1. Ports 0x3F8 through 0x3FF are the eight
/// registers of the first serial port.
pub const COM1_PORT_BASE: u16 = 0x3F8;

/// Number of I/O ports the 16550 uses (8 registers).
pub const COM1_PORT_COUNT: u16 = 8;

/// Check whether an I/O port is in the COM1 serial range.
pub fn is_serial_port(port: u16) -> bool {
    (COM1_PORT_BASE..COM1_PORT_BASE + COM1_PORT_COUNT).contains(&port)
}

/// Create a new serial device with stdout as the output sink.
///
/// The `EventFd` is the interrupt trigger — when the serial device wants to
/// raise an interrupt (e.g., "received a byte"), it writes to this fd.
/// For the MVP, we never poll this fd because we don't deliver serial
/// interrupts to the guest. The fd exists because vm-superio's Serial
/// requires a Trigger, but it's effectively unused.
pub fn create() -> io::Result<Serial<EventFdTrigger, NoEvents, Stdout>> {
    // EFD_NONBLOCK: reads from the eventfd return immediately with EAGAIN
    // instead of blocking. We never read it, but nonblocking is safer.
    let interrupt_evt = EventFdTrigger::new(libc::EFD_NONBLOCK)?;

    // Serial::new wires the trigger to a NoEvents (no-op) event handler and
    // our stdout sink. The NoEvents type parameter is inferred from this call.
    Ok(Serial::new(interrupt_evt, io::stdout()))
}

/// Handle an I/O out (write) from the guest to a serial port.
///
/// Called from the vCPU run loop when the guest executes an `out`
/// instruction to a port in the COM1 range. The `data` slice contains
/// the byte(s) being written.
pub fn handle_write(serial: &mut Serial<EventFdTrigger, NoEvents, Stdout>, port: u16, data: &[u8]) {
    let offset = u8::try_from(port - COM1_PORT_BASE).expect("serial offset is within COM1 range");
    for &byte in data {
        // serial.write() handles the UART register logic: if offset is 0
        // (THR), it writes the byte to our stdout sink. If offset is
        // something else (IER, FCR, LCR, MCR), it updates internal state.
        let _ = serial.write(offset, byte);
    }
    // Flush stdout so characters appear immediately rather than being
    // buffered. The kernel often writes one character at a time during
    // early boot, and buffering would make output appear in chunks.
    let _ = io::stdout().flush();
}

/// Handle an I/O in (read) from the guest from a serial port.
///
/// Called from the vCPU run loop when the guest executes an `in`
/// instruction from a port in the COM1 range. We fill `data` with
/// the value the UART register should return.
pub fn handle_read(
    serial: &mut Serial<EventFdTrigger, NoEvents, Stdout>,
    port: u16,
    data: &mut [u8],
) {
    let offset = u8::try_from(port - COM1_PORT_BASE).expect("serial offset is within COM1 range");
    for byte in data.iter_mut() {
        // serial.read() returns the current value of the addressed register.
        // Most importantly, LSR (offset 5) returns the line status with the
        // "transmitter empty" and "transmitter holding register empty" bits
        // set, telling the kernel it can send another byte immediately.
        *byte = serial.read(offset);
    }
}
