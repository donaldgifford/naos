//! naos-linux: minimum viable KVM-based hypervisor.
//!
//! See `docs/decisions/` for the design doc (DESIGN-naos-linux) and
//! `DEVELOPMENT.md` at the repo root for build and run instructions.
//!
//! The implementation is organized into six files, one per responsibility:
//!
//!   - `main.rs`   — argument parsing, construct Vmm, handle top-level errors
//!   - `vmm.rs`    — Vmm struct: owns kvm, vm, memory, vcpu, serial
//!   - `memory.rs` — guest memory: mmap region + KVM registration
//!   - `kernel.rs` — vmlinux ELF loader, returns kernel entry address
//!   - `boot.rs`   — GDT, page tables, sregs, regs (heavily commented)
//!   - `vcpu.rs`   — vCPU run loop and exit dispatch
//!   - `serial.rs` — 16550 UART wired to stdout
//!
//! None of these modules exist yet. This stub is the starting point.

fn main() {
    println!("naos-linux: stub entry point.");
    println!("Implementation begins here. See DESIGN-naos-linux.");
}
