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

#[cfg(test)]
mod tests {
    use super::load;
    use std::path::PathBuf;
    use vm_memory::{Address, GuestAddress, GuestMemoryMmap};

    const ELF_HEADER_LEN: u64 = 64;
    const PROGRAM_HEADER_LEN: u64 = 56;

    fn guest_mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 32 * 1024 * 1024)]).unwrap()
    }

    /// A unique temp path per process+test so parallel tests never collide.
    fn temp_path(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("naos_{}_{}.bin", tag, std::process::id()));
        path
    }

    /// Build the smallest valid `x86_64` ELF the loader will accept: an ELF64
    /// header plus a single `PT_LOAD` segment of NOPs at `entry`.
    fn minimal_elf64(entry: u64) -> Vec<u8> {
        let segment = [0x90_u8; 16]; // NOPs
        let p_offset = ELF_HEADER_LEN + PROGRAM_HEADER_LEN;

        let mut elf = Vec::new();
        // e_ident: magic, ELFCLASS64, little-endian, version, System V ABI.
        elf.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
        elf.extend_from_slice(&[0; 8]); // ABI version + padding (16 bytes total)
        elf.extend_from_slice(&2_u16.to_le_bytes()); // e_type = ET_EXEC
        elf.extend_from_slice(&62_u16.to_le_bytes()); // e_machine = EM_X86_64
        elf.extend_from_slice(&1_u32.to_le_bytes()); // e_version
        elf.extend_from_slice(&entry.to_le_bytes()); // e_entry
        elf.extend_from_slice(&ELF_HEADER_LEN.to_le_bytes()); // e_phoff
        elf.extend_from_slice(&0_u64.to_le_bytes()); // e_shoff
        elf.extend_from_slice(&0_u32.to_le_bytes()); // e_flags
        elf.extend_from_slice(&64_u16.to_le_bytes()); // e_ehsize
        elf.extend_from_slice(&56_u16.to_le_bytes()); // e_phentsize
        elf.extend_from_slice(&1_u16.to_le_bytes()); // e_phnum
        elf.extend_from_slice(&0_u16.to_le_bytes()); // e_shentsize
        elf.extend_from_slice(&0_u16.to_le_bytes()); // e_shnum
        elf.extend_from_slice(&0_u16.to_le_bytes()); // e_shstrndx
        assert_eq!(elf.len() as u64, ELF_HEADER_LEN);

        // One PT_LOAD program header.
        elf.extend_from_slice(&1_u32.to_le_bytes()); // p_type = PT_LOAD
        elf.extend_from_slice(&5_u32.to_le_bytes()); // p_flags = R+X
        elf.extend_from_slice(&p_offset.to_le_bytes()); // p_offset
        elf.extend_from_slice(&entry.to_le_bytes()); // p_vaddr
        elf.extend_from_slice(&entry.to_le_bytes()); // p_paddr
        elf.extend_from_slice(&16_u64.to_le_bytes()); // p_filesz
        elf.extend_from_slice(&16_u64.to_le_bytes()); // p_memsz
        elf.extend_from_slice(&0x1000_u64.to_le_bytes()); // p_align
        assert_eq!(elf.len() as u64, p_offset);

        elf.extend_from_slice(&segment);
        elf
    }

    #[test]
    fn load_fails_on_missing_file() {
        let err = load(&guest_mem(), &PathBuf::from("/nonexistent/naos/vmlinux")).unwrap_err();
        assert!(err.to_string().contains("Failed to open kernel file"));
    }

    #[test]
    fn load_fails_on_a_non_elf_file() {
        let path = temp_path("not_elf");
        std::fs::write(&path, b"definitely not an ELF binary").unwrap();
        let result = load(&guest_mem(), &path);
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
    }

    #[test]
    fn load_returns_entry_address_of_a_valid_elf() {
        let entry = 0x10_0000;
        let path = temp_path("minimal_elf");
        std::fs::write(&path, minimal_elf64(entry)).unwrap();
        let result = load(&guest_mem(), &path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(result.unwrap().raw_value(), entry);
    }
}
