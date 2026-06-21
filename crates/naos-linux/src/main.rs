// main.rs
//
// CLI entry point for naos-linux.
//
// Parses three arguments (kernel path, memory size, optional cmdline),
// builds the VMM, and runs it. Errors propagate via anyhow and are
// printed to stderr on exit.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

mod boot;
mod kernel;
mod memory;
mod serial;
mod vcpu;
mod vmm;

/// naos-linux: minimum viable KVM-based hypervisor.
///
/// Boots a vmlinux ELF kernel under KVM and prints its output to stdout.
/// The kernel will panic when it cannot find an init process — that panic
/// is the success signal. See DESIGN-naos-linux for the full rationale.
#[derive(Parser, Debug)]
#[command(name = "naos-linux")]
struct Args {
    /// Path to a vmlinux ELF file.
    #[arg(long)]
    kernel: PathBuf,

    /// Guest RAM in MiB.
    #[arg(long, default_value_t = 256)]
    mem: u64,

    /// Kernel command line.
    #[arg(long, default_value = "console=ttyS0 reboot=k panic=1 pci=off")]
    cmdline: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let mut vmm = vmm::Vmm::new(&args.kernel, args.mem, &args.cmdline)?;

    vmm.run()
}
