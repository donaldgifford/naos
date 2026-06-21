// vmm.rs
//
// The Vmm struct: orchestrates initialization and owns all VMM resources.
//
// Initialization follows a strict order dictated by KVM's API constraints.
// Each step is annotated with why it happens where it does.

use std::io::Stdout;
use std::path::Path;

use anyhow::{Context, Result};
use kvm_ioctls::{Kvm, VcpuFd, VmFd};
use vm_memory::{Address, GuestMemoryMmap};
use vm_superio::serial::{NoEvents, Serial};

use crate::serial::EventFdTrigger;
use crate::{boot, kernel, memory, serial, vcpu};

/// The Vmm struct owns all resources for a single virtual machine.
///
/// Ownership is important: the `VmFd` must outlive the `VcpuFd` (KVM enforces
/// this at the kernel level), and the `GuestMemoryMmap` must outlive the `VmFd`
/// (because KVM holds a reference to our mmap'd memory). Rust's ownership
/// model enforces this naturally — fields are dropped in declaration order
/// (last declared, first dropped), so we declare them in dependency order.
pub struct Vmm {
    // The Kvm handle holds the /dev/kvm fd. Not used after init, but must
    // stay alive for the lifetime of the VM.
    _kvm: Kvm,

    // The VM fd. Must outlive vCPU and memory.
    _vm: VmFd,

    // Guest memory. Must outlive the VM fd (KVM references it).
    _guest_mem: GuestMemoryMmap,

    // The vCPU fd. Used in the run loop.
    vcpu: VcpuFd,

    // The serial device. Used in the run loop to handle I/O exits.
    serial: Serial<EventFdTrigger, NoEvents, Stdout>,
}

impl Vmm {
    /// Create and fully initialize the VMM.
    ///
    /// After this returns, the vCPU is configured and ready to run.
    /// Call `self.run()` to start executing guest code.
    pub fn new(kernel_path: &Path, mem_mib: u64, cmdline: &str) -> Result<Self> {
        let mem_bytes = mem_mib * 1024 * 1024;

        // --- Step 1: Open /dev/kvm ---
        // Kvm::new() opens /dev/kvm and checks the API version.
        // Fails if KVM is not available (module not loaded, no permissions,
        // not a Linux host, etc.)
        let kvm = Kvm::new().context("Failed to open /dev/kvm")?;

        // --- Step 2: Create the VM ---
        // Creates a VM fd. This is the container for everything else:
        // memory, vCPUs, IRQ chip, etc.
        let vm = kvm.create_vm().context("Failed to create VM")?;

        // --- Step 3: Set TSS address ---
        // Required on Intel (VT-x) before creating vCPUs. The Task State
        // Segment is a legacy x86 structure that KVM needs a guest physical
        // address for. 0xFFFB_D000 is a conventional address in the high
        // area that won't conflict with guest RAM (our RAM ends at 256 MiB).
        // On AMD (SVM), this ioctl is a no-op but doesn't error.
        //
        // KVM API docs: KVM_SET_TSS_ADDR, section 4.4.
        vm.set_tss_address(0xFFFB_D000)
            .context("Failed to set TSS address")?;

        // --- Step 4: Create in-kernel IRQ chip ---
        // Sets up the emulated PIC (8259) and IOAPIC inside KVM.
        // Not strictly needed for the MVP (no devices raise interrupts),
        // but creating it now costs nothing and some kernel configurations
        // probe for it during early boot.
        //
        // Must be done before creating vCPUs.
        vm.create_irq_chip()
            .context("Failed to create in-kernel IRQ chip")?;

        // --- Step 5: Allocate and register guest memory ---
        let guest_mem = memory::build(mem_mib)?;
        memory::register(&vm, &guest_mem)?;

        // --- Step 6: Load the kernel ---
        // Copies ELF segments into guest memory and returns the entry point.
        let entry_addr = kernel::load(&guest_mem, kernel_path)?;

        // --- Step 7: Write boot parameters and command line ---
        boot::write_cmdline(&guest_mem, cmdline)?;
        boot::write_boot_params(&guest_mem, cmdline, mem_bytes)?;

        // --- Step 8: Create the serial device ---
        let serial_dev = serial::create().context("Failed to create serial device")?;

        // --- Step 9: Create and configure the vCPU ---
        // create_vcpu takes the vCPU index (0 for the first and only vCPU).
        let vcpu = vm.create_vcpu(0).context("Failed to create vCPU")?;

        // --- Step 9b: Seed the guest CPUID ---
        // KVM gives a freshly created vCPU an *empty* CPUID table. The kernel
        // reads CPUID during the very first instructions of boot — in
        // common_startup_64 it reads leaf 0x80000001 to decide which EFER bits
        // (NXE, etc.) to set, then issues `wrmsr` to EFER. With no CPUID, that
        // wrmsr writes a value KVM rejects with #GP, which (before the kernel's
        // real IDT exists) cascades into a triple fault — the guest halts via
        // KVM_EXIT_SHUTDOWN before printing a single byte. Copying the host's
        // KVM-supported CPUID into the vCPU is what every KVM VMM does here.
        let kvm_cpuid = kvm
            .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
            .context("Failed to query KVM-supported CPUID")?;
        vcpu.set_cpuid2(&kvm_cpuid)
            .context("Failed to set guest CPUID")?;

        // Configure CPU registers: GDT, page tables, control registers,
        // segment registers, RIP, RSI, RFLAGS.
        boot::configure(&vcpu, &guest_mem, entry_addr.raw_value())?;

        Ok(Self {
            _kvm: kvm,
            _vm: vm,
            _guest_mem: guest_mem,
            vcpu,
            serial: serial_dev,
        })
    }

    /// Run the VM until the guest halts or an error occurs.
    pub fn run(&mut self) -> Result<()> {
        vcpu::run(&mut self.vcpu, &mut self.serial)
    }
}
