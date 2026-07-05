// vcpu.rs
//
// The vCPU run loop: the core of the VMM.
//
// This module is a single function: run the vCPU in a loop, dispatching
// vmexits to the appropriate handler. The loop exits when the guest halts
// or when we encounter an unexpected exit reason.
//
// For the MVP, this is a single-threaded blocking loop. The vCPU thread
// and the I/O handling thread are the same thread. This is fine because:
// - We have one vCPU (no SMP)
// - Our only device (serial) does synchronous I/O (stdout writes)
// - We don't need to poll for incoming I/O (no serial input)
//
// Adding a second device or serial input would force us to split into
// a vCPU thread and an I/O thread with an event loop. That's MVP+1.

use std::io::Stdout;

use anyhow::{Context, Result, bail};
use kvm_ioctls::VcpuFd;
use vm_superio::serial::{NoEvents, Serial};

use crate::serial;
use crate::serial::EventFdTrigger;

/// Run the vCPU until the guest halts or an unhandled exit occurs.
///
/// This function does not return until the VM is done. On success (guest
/// executed HLT), it returns Ok(()). On unexpected exits, it returns an
/// error describing the exit reason.
pub fn run(
    vcpu: &mut VcpuFd,
    serial_dev: &mut Serial<EventFdTrigger, NoEvents, Stdout>,
) -> Result<()> {
    loop {
        // KVM_RUN: enter the guest. This blocks until a vmexit occurs.
        // The guest could run for microseconds or milliseconds depending
        // on what it's doing. During kernel boot, exits are frequent
        // because the kernel writes many characters to the serial port.
        match vcpu.run().context("KVM_RUN failed")? {
            // --- I/O port exits ---
            // The guest executed `out <port>, <data>` (write to I/O port).
            kvm_ioctls::VcpuExit::IoOut(port, data) => {
                if serial::is_serial_port(port) {
                    serial::handle_write(serial_dev, port, data);
                } else if is_reset_request(port, data) {
                    // The guest asked the platform to reset. After the panic,
                    // `reboot=k` pulses the 8042 keyboard-controller reset line
                    // (out 0xFE -> port 0x64); other configs poke the PCI reset
                    // register at 0xCF9. We have no platform to reboot, so a
                    // reset request is our cue to stop — the same clean exit as
                    // Hlt. Without this the kernel spins forever retrying the
                    // reset and naos never returns.
                    break;
                }
                // Any other port is silently ignored. A real VMM would log
                // these; for the MVP the kernel probes ports we don't emulate
                // (e.g. 0x80 POST codes, CMOS/RTC) and we must not crash on them.
            }

            // The guest executed `in <port>` (read from I/O port).
            kvm_ioctls::VcpuExit::IoIn(port, data) => {
                if serial::is_serial_port(port) {
                    serial::handle_read(serial_dev, port, data);
                } else {
                    // Return 0xFF for unknown ports. This is conventional —
                    // on real hardware, reading a non-existent port returns
                    // all-ones. It tells the kernel "nothing here."
                    for byte in data.iter_mut() {
                        *byte = 0xFF;
                    }
                }
            }

            // --- Guest halted or shut down ---
            // Hlt: the kernel executed HLT. After a panic with panic=1 on the
            // cmdline it spins in an infinite HLT loop — our clean exit signal.
            // Shutdown: triple fault or ACPI shutdown, also a clean exit for us.
            kvm_ioctls::VcpuExit::Hlt | kvm_ioctls::VcpuExit::Shutdown => {
                break;
            }

            // --- Everything else ---
            // Any vmexit we don't handle is a bug. Log the variant and
            // bail so we can diagnose what the guest was trying to do.
            exit => {
                bail!("Unexpected vCPU exit: {exit:?}");
            }
        }
    }

    Ok(())
}

/// The 8042 keyboard-controller command port. `reboot=k` resets the machine by
/// writing the "pulse reset line" command (0xFE) here.
const KBD_CMD_PORT: u16 = 0x64;
const KBD_RESET_CMD: u8 = 0xFE;

/// The PCI reset control register (a.k.a. "reset control register", `RST_CNT`).
/// Writing a value with the system-reset bit (bit 2) set requests a reset; the
/// full-reset bit (bit 3) selects cold vs warm. Used by `reboot=p`/`reboot=c`.
const PCI_RESET_PORT: u16 = 0x0CF9;
const PCI_RESET_BIT: u8 = 1 << 2;

/// Does this port write represent a platform reset request?
///
/// The MVP emulates no reboot-capable hardware, so we recognize the two reset
/// mechanisms a Linux guest reaches for and treat either as "the guest wants to
/// power-cycle" — which, with nothing to reset, means we stop the VM cleanly.
fn is_reset_request(port: u16, data: &[u8]) -> bool {
    match (port, data) {
        (KBD_CMD_PORT, [KBD_RESET_CMD, ..]) => true,
        (PCI_RESET_PORT, [value, ..]) => value & PCI_RESET_BIT != 0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{KBD_CMD_PORT, KBD_RESET_CMD, PCI_RESET_BIT, PCI_RESET_PORT, is_reset_request};

    /// The full-reset bit (cold vs warm) accompanies the system-reset bit on the
    /// PCI reset register; it must not, on its own, count as a reset request.
    const PCI_FULL_RESET_BIT: u8 = 1 << 3;

    #[test]
    fn kbd_reset_command_is_a_reset() {
        // `reboot=k`: pulse the CPU reset line via the 8042 command port.
        assert!(is_reset_request(KBD_CMD_PORT, &[KBD_RESET_CMD]));
    }

    #[test]
    fn other_kbd_commands_are_not_resets() {
        // 0xAD = "disable keyboard interface" — a normal 8042 command.
        assert!(!is_reset_request(KBD_CMD_PORT, &[0xAD]));
        assert!(!is_reset_request(KBD_CMD_PORT, &[0x00]));
    }

    #[test]
    fn pci_register_with_system_reset_bit_is_a_reset() {
        assert!(is_reset_request(PCI_RESET_PORT, &[PCI_RESET_BIT]));
        // Cold reset also sets the system-reset bit, so it counts too.
        assert!(is_reset_request(
            PCI_RESET_PORT,
            &[PCI_RESET_BIT | PCI_FULL_RESET_BIT]
        ));
    }

    #[test]
    fn pci_register_without_system_reset_bit_is_not_a_reset() {
        assert!(!is_reset_request(PCI_RESET_PORT, &[0x00]));
        // The full-reset bit alone (no system-reset bit) is not a reset.
        assert!(!is_reset_request(PCI_RESET_PORT, &[PCI_FULL_RESET_BIT]));
    }

    #[test]
    fn unrelated_ports_are_never_resets() {
        // Port 0x80 (POST codes) and the serial range carry the reset byte
        // value during boot but must not be treated as reset requests.
        assert!(!is_reset_request(0x0080, &[KBD_RESET_CMD]));
        for port in 0x03F8_u16..=0x03FF {
            assert!(!is_reset_request(port, &[KBD_RESET_CMD]));
        }
    }

    #[test]
    fn empty_write_is_not_a_reset() {
        assert!(!is_reset_request(KBD_CMD_PORT, &[]));
        assert!(!is_reset_request(PCI_RESET_PORT, &[]));
    }

    // Exercises the real run loop to its terminal Hlt arm. Requires /dev/kvm;
    // skips cleanly when unavailable. Run under sudo to exercise it.
    #[test]
    fn run_returns_when_the_guest_halts() {
        use vm_memory::{Bytes, GuestAddress};

        let Ok(kvm) = kvm_ioctls::Kvm::new() else {
            eprintln!("skipping run test: /dev/kvm not accessible");
            return;
        };
        let vm = kvm.create_vm().unwrap();
        vm.set_tss_address(0xFFFB_D000).unwrap();
        let mem = crate::memory::build(2).unwrap();
        crate::memory::register(&vm, &mem).unwrap();

        // A single HLT (0xF4) at the entry point. With no in-kernel IRQ chip the
        // guest's HLT exits to userspace, so run() takes its Hlt arm and returns.
        let entry = 0x10_0000;
        mem.write_obj(0xF4_u8, GuestAddress(entry)).unwrap();

        let mut vcpu = vm.create_vcpu(0).unwrap();
        let cpuid = kvm
            .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
            .unwrap();
        vcpu.set_cpuid2(&cpuid).unwrap();
        crate::boot::configure(&vcpu, &mem, entry).unwrap();

        let mut serial = crate::serial::create().unwrap();
        super::run(&mut vcpu, &mut serial).unwrap();
    }

    // Exercises the run loop's defensive arm: an exit reason we do not handle
    // must surface as an error rather than being swallowed. Requires /dev/kvm.
    #[test]
    fn run_errors_on_an_unhandled_exit() {
        use vm_memory::{Bytes, GuestAddress};

        let Ok(kvm) = kvm_ioctls::Kvm::new() else {
            eprintln!("skipping run test: /dev/kvm not accessible");
            return;
        };
        let vm = kvm.create_vm().unwrap();
        vm.set_tss_address(0xFFFB_D000).unwrap();
        let mem = crate::memory::build(2).unwrap();
        crate::memory::register(&vm, &mem).unwrap();

        // `mov al, [0x200000]`: the page tables map 0x200000 but no memory slot
        // backs it (guest RAM is 2 MiB), so the read exits with MMIO — a reason
        // run() does not handle and must therefore turn into an error.
        let entry = 0x10_0000;
        let code = [0xA0_u8, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00];
        mem.write_slice(&code, GuestAddress(entry)).unwrap();

        let mut vcpu = vm.create_vcpu(0).unwrap();
        let cpuid = kvm
            .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
            .unwrap();
        vcpu.set_cpuid2(&cpuid).unwrap();
        crate::boot::configure(&vcpu, &mem, entry).unwrap();

        let mut serial = crate::serial::create().unwrap();
        assert!(super::run(&mut vcpu, &mut serial).is_err());
    }
}
