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

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn defaults_apply_when_only_kernel_is_given() {
        let args = Args::try_parse_from(["naos-linux", "--kernel", "/k"]).unwrap();
        assert_eq!(args.kernel, PathBuf::from("/k"));
        assert_eq!(args.mem, 256);
        assert_eq!(args.cmdline, "console=ttyS0 reboot=k panic=1 pci=off");
    }

    #[test]
    fn explicit_flags_override_defaults() {
        let args = Args::try_parse_from([
            "naos-linux",
            "--kernel",
            "/k",
            "--mem",
            "512",
            "--cmdline",
            "quiet",
        ])
        .unwrap();
        assert_eq!(args.mem, 512);
        assert_eq!(args.cmdline, "quiet");
    }

    #[test]
    fn kernel_argument_is_required() {
        assert!(Args::try_parse_from(["naos-linux"]).is_err());
    }

    #[test]
    fn non_numeric_mem_is_rejected() {
        assert!(Args::try_parse_from(["naos-linux", "--kernel", "/k", "--mem", "lots"]).is_err());
    }
}
