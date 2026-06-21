// kernel.rs
//
// Kernel loading: parse a vmlinux ELF binary and copy its segments
// into guest memory. Returns the entry point address (where RIP should
// point when the vCPU starts).
//
// This module is deliberately minimal — linux-loader does the heavy lifting.
// We wrap it to provide naos-specific error context and to isolate the
// linux-loader API surface so the rest of the VMM doesn't depend on it.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};
use linux_loader::loader::KernelLoader;
use linux_loader::loader::elf::Elf as ElfLoader;
use vm_memory::{GuestAddress, GuestMemoryMmap};

/// Load a vmlinux ELF binary into guest memory.
///
/// Parses the ELF program headers, copies each `PT_LOAD` segment into guest
/// memory at the physical addresses specified in the headers, and returns
/// the kernel's entry point address (the ELF `e_entry` field).
///
/// The kernel is loaded wherever the ELF says — we do not choose the
/// address. A typical tinyconfig vmlinux loads at 0x1000000 (16 MiB),
/// but this is determined by the kernel's linker script, not by us.
pub fn load(guest_mem: &GuestMemoryMmap, kernel_path: &Path) -> Result<GuestAddress> {
    let mut kernel_file = File::open(kernel_path).context("Failed to open kernel file")?;

    // ElfLoader::load arguments:
    //   guest_mem     — where to copy segments into
    //   kernel_start  — None means "use the addresses from the ELF headers"
    //                   (Some would override, but we want the ELF's own layout)
    //   kernel_image  — the file to read from
    //   himem_start   — None means "no high-memory constraint"
    //                   (used for bzImage; irrelevant for raw ELF)
    let loader_result = ElfLoader::load(guest_mem, None, &mut kernel_file, None)
        .context("Failed to load vmlinux ELF")?;

    // loader_result.kernel_load is the ELF entry point (e_entry).
    // This is the guest physical address where we point RIP.
    Ok(loader_result.kernel_load)
}
