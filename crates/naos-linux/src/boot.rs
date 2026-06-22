// boot.rs
//
// Boot environment setup: everything the CPU and kernel need to start.
//
// This file is the most complex in naos-linux and the one most likely to
// contain subtle bugs. Every constant is annotated with its specification
// reference so future readers can audit the code against the manual.
//
// The boot setup has four parts:
// 1. Boot parameters (struct boot_params / "zero page")
// 2. GDT (Global Descriptor Table)
// 3. Page tables (4-level, identity-mapped, 2 MiB pages)
// 4. CPU registers (control registers, segment registers, general registers)

use anyhow::{Context, Result};
use kvm_bindings::{kvm_regs, kvm_segment};
use kvm_ioctls::VcpuFd;
use vm_memory::{ByteValued, Bytes, GuestAddress, GuestMemoryMmap};

// ---------------------------------------------------------------------------
// Guest physical addresses. See "The guest physical memory map" in the
// walkthrough document for the full layout and rationale for each address.
// ---------------------------------------------------------------------------

const GDT_ADDR: u64 = 0x500;
const PML4_ADDR: u64 = 0x1000;
const PDPT_ADDR: u64 = 0x2000;
const PD_ADDR: u64 = 0x3000;
const BOOT_PARAMS_ADDR: u64 = 0x7000;
pub const CMDLINE_ADDR: u64 = 0x2_0000;

// ---------------------------------------------------------------------------
// GDT entries. See walkthrough Part 2 for the bit-by-bit derivation.
// ---------------------------------------------------------------------------

/// Null descriptor. Required by the CPU as GDT entry 0.
/// Intel SDM Vol. 3A, Section 3.4.5: "The first descriptor in the GDT is
/// not used by the processor. [...] A null selector [...] points to the
/// first entry."
const GDT_NULL: u64 = 0;

/// 64-bit kernel code segment.
///   Base=0, Limit=0xFFFFF, G=1, L=1, D=0, P=1, DPL=0, S=1, Type=0xA
/// Intel SDM Vol. 3A, Section 5.2.1: "When L=1 and D=0, the code segment
/// is a 64-bit code segment."
const GDT_CODE: u64 = 0x00AF_9A00_0000_FFFF;

/// Kernel data segment.
///   Base=0, Limit=0xFFFFF, G=1, D=1, L=0, P=1, DPL=0, S=1, Type=0x2
/// In long mode, DS/ES/SS base and limit are ignored, but the segment
/// must be present (P=1) or the CPU faults on load.
const GDT_DATA: u64 = 0x00CF_9200_0000_FFFF;

// Selectors are byte offsets into the GDT.
// Entry 1 is at offset 8 (0x08), entry 2 at offset 16 (0x10).
const CODE_SEL: u16 = 0x08;
const DATA_SEL: u16 = 0x10;

// ---------------------------------------------------------------------------
// Page table flags.
// Intel SDM Vol. 3A, Section 4.5, Table 4-14 through 4-19.
// ---------------------------------------------------------------------------

/// Present bit. Must be 1 for valid entries.
const PTE_PRESENT: u64 = 1 << 0;
/// Read/write bit. 1 = writable.
const PTE_RW: u64 = 1 << 1;
/// Page size bit (PD entry only). 1 = 2 MiB large page.
const PTE_PS: u64 = 1 << 7;

// ---------------------------------------------------------------------------
// Control register bits.
// ---------------------------------------------------------------------------

/// CR0 bits. Intel SDM Vol. 3A, Section 2.5.
const CR0_PE: u64 = 1 << 0; // Protection Enable
const CR0_MP: u64 = 1 << 1; // Monitor Coprocessor
const CR0_ET: u64 = 1 << 4; // Extension Type (hardwired to 1)
const CR0_NE: u64 = 1 << 5; // Numeric Error
const CR0_PG: u64 = 1 << 31; // Paging

/// CR4 bits. Intel SDM Vol. 3A, Section 2.5.
const CR4_PAE: u64 = 1 << 5; // Physical Address Extension

/// EFER bits. Intel SDM Vol. 3A, Section 2.2.1.
const EFER_LME: u64 = 1 << 8; // Long Mode Enable
const EFER_LMA: u64 = 1 << 10; // Long Mode Active

// ---------------------------------------------------------------------------
// The e820 memory map type for usable RAM.
// Defined in arch/x86/include/uapi/asm/e820.h in the kernel source.
// ---------------------------------------------------------------------------

const E820_RAM: u32 = 1;

// e820 region boundaries for the canonical PC memory split (see the e820 setup
// in `write_boot_params` for why two regions are mandatory).
/// Top of conventional memory (640 KiB).
const LOW_RAM_END: u64 = 0x000A_0000;
/// Base of extended memory (1 MiB). The 640 KiB-1 MiB gap is the legacy
/// VGA/BIOS hole and is intentionally left unmapped.
const EXT_RAM_START: u64 = 0x0010_0000;

/// Write the kernel command line into guest memory.
///
/// The cmdline is a null-terminated ASCII string. Its address is recorded
/// in `boot_params` so the kernel can find it.
pub fn write_cmdline(guest_mem: &GuestMemoryMmap, cmdline: &str) -> Result<()> {
    let cmdline_bytes = cmdline.as_bytes();
    guest_mem
        .write_slice(cmdline_bytes, GuestAddress(CMDLINE_ADDR))
        .context("Failed to write cmdline to guest memory")?;
    // Null terminator.
    guest_mem
        .write_obj(0u8, GuestAddress(CMDLINE_ADDR + cmdline_bytes.len() as u64))
        .context("Failed to write cmdline null terminator")?;
    Ok(())
}

/// Newtype wrapper that lets us write a `boot_params` into guest memory.
///
/// `GuestMemory::write_obj` requires its argument to implement vm-memory's
/// `ByteValued` (a plain-old-data marker trait). linux-loader's `boot_params`
/// is a `#[repr(C, packed)]` POD struct but does not implement `ByteValued`,
/// and the orphan rule forbids us from implementing a foreign trait on a
/// foreign type. Wrapping it in our own type lets us provide the impl. This is
/// the same pattern Firecracker uses for its zero page.
// The field is only ever read through the ByteValued impl (as raw bytes), never
// by name, so the dead-code lint can't see the use. linux-loader silences the
// same warning on its own ByteValued wrappers.
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct BootParamsWrapper(linux_loader::bootparam::boot_params);

// SAFETY: boot_params is a #[repr(C, packed)] struct of integers and fixed-size
// arrays — no pointers and no padding-dependent invariants — so every byte
// pattern is a valid value and reading it as a raw byte slice is sound.
unsafe impl ByteValued for BootParamsWrapper {}

/// Build and write the boot parameters ("zero page") into guest memory.
///
/// This provides the kernel with its memory map and command line pointer.
/// Without this, the kernel has no idea how much RAM exists and cannot
/// configure the serial console.
pub fn write_boot_params(guest_mem: &GuestMemoryMmap, cmdline: &str, mem_size: u64) -> Result<()> {
    // Start with a zeroed boot_params struct. linux-loader provides the
    // Rust definition of this struct, matching the kernel's bootparam.h.
    let mut params = linux_loader::bootparam::boot_params::default();

    // --- Command line ---
    // cmd_line_ptr is a u32 physical address. CMDLINE_ADDR is well below 4 GiB,
    // and a kernel command line never approaches 4 GiB, so both conversions are
    // exact — try_from documents that and would surface a bug if it ever weren't.
    params.hdr.cmd_line_ptr = u32::try_from(CMDLINE_ADDR).expect("CMDLINE_ADDR fits in u32");
    params.hdr.cmdline_size = u32::try_from(cmdline.len()).expect("cmdline length fits in u32");

    // --- Boot protocol ---
    // type_of_loader: non-zero means "a bootloader loaded us." The kernel
    // checks this to decide whether boot_params is trustworthy.
    // 0xFF = "undefined" bootloader, which is fine for our purposes.
    params.hdr.type_of_loader = 0xFF;

    // --- e820 memory map ---
    // The kernel REJECTS the whole e820 table if it has fewer than two entries:
    // arch/x86/kernel/e820.c's append_e820_table() does `if (nr_entries < 2)
    // return -1;`, treating a single region as bogus. On that path the kernel
    // falls back to legacy e801 detection, finds only the low 640 KiB, decides
    // its own .text/.data/.bss at 16 MiB "are not marked as E820_TYPE_RAM", and
    // panics in alloc_low_pages. So we must hand it at least two regions.
    //
    // We use the canonical PC split that real firmware reports: conventional
    // memory below 640 KiB, then extended memory from 1 MiB to the top of guest
    // RAM, leaving the legacy 640 KiB-1 MiB hole (VGA/BIOS) unmapped. The kernel
    // loads at 16 MiB, which lives in the second (extended) region.
    params.e820_table[0] = linux_loader::bootparam::boot_e820_entry {
        addr: 0,
        size: LOW_RAM_END,
        type_: E820_RAM,
    };
    params.e820_table[1] = linux_loader::bootparam::boot_e820_entry {
        addr: EXT_RAM_START,
        size: mem_size.saturating_sub(EXT_RAM_START),
        type_: E820_RAM,
    };
    params.e820_entries = 2;

    // Write the completed boot_params to guest memory at BOOT_PARAMS_ADDR.
    // Wrap it so it satisfies write_obj's ByteValued bound (see BootParamsWrapper).
    guest_mem
        .write_obj(BootParamsWrapper(params), GuestAddress(BOOT_PARAMS_ADDR))
        .context("Failed to write boot_params to guest memory")?;

    Ok(())
}

/// Write the GDT into guest memory.
///
/// Three entries (null, code, data) = 24 bytes at `GDT_ADDR`.
fn write_gdt(guest_mem: &GuestMemoryMmap) -> Result<()> {
    let gdt_entries: [u64; 3] = [GDT_NULL, GDT_CODE, GDT_DATA];

    for (i, entry) in gdt_entries.iter().enumerate() {
        let addr = GDT_ADDR + (i as u64) * 8;
        guest_mem
            .write_obj(*entry, GuestAddress(addr))
            .context("Failed to write GDT entry")?;
    }

    Ok(())
}

/// Build identity-mapped page tables covering the first 1 GiB.
///
/// We use 2 MiB large pages (PS bit set in PD entries) so we only need
/// three pages of tables: one PML4, one PDPT, one PD. The PD has 512
/// entries × 2 MiB = 1 GiB of coverage, more than enough for 256 MiB.
fn write_page_tables(guest_mem: &GuestMemoryMmap) -> Result<()> {
    // PML4[0] → points to PDPT
    guest_mem
        .write_obj(PDPT_ADDR | PTE_PRESENT | PTE_RW, GuestAddress(PML4_ADDR))
        .context("Failed to write PML4 entry")?;

    // PDPT[0] → points to PD
    guest_mem
        .write_obj(PD_ADDR | PTE_PRESENT | PTE_RW, GuestAddress(PDPT_ADDR))
        .context("Failed to write PDPT entry")?;

    // PD[0..511] → identity-mapped 2 MiB pages
    // Entry i maps virtual address [i * 2MiB, (i+1) * 2MiB) to the same
    // physical address. PTE_PS marks these as 2 MiB large pages so the CPU
    // doesn't try to walk a fourth table level (PT).
    for i in 0u64..512 {
        let phys_addr = i * (2 * 1024 * 1024); // i * 2 MiB
        let entry = phys_addr | PTE_PRESENT | PTE_RW | PTE_PS;
        let addr = PD_ADDR + i * 8;
        guest_mem
            .write_obj(entry, GuestAddress(addr))
            .context("Failed to write PD entry")?;
    }

    Ok(())
}

/// Helper: build a `kvm_segment` struct from a GDT descriptor and selector.
///
/// KVM's `kvm_segment` has many fields that could be derived from the raw
/// GDT entry, but KVM wants them spelled out explicitly in the struct.
/// This helper constructs the struct for our two segment types.
fn make_code_segment() -> kvm_segment {
    kvm_segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: CODE_SEL,
        // Type field encodes: execute(1), conforming(0), readable(1), accessed(1)
        // = 0xB. Some references use 0xA or 0xB; the accessed bit is set by
        // the CPU on first use, but KVM expects us to pre-set it.
        type_: 0xB,
        present: 1,
        dpl: 0, // Ring 0 (kernel)
        db: 0,  // Must be 0 for 64-bit code (Intel SDM Vol. 3A, 5.2.1)
        s: 1,   // Code/data segment (not system)
        l: 1,   // 64-bit code segment
        g: 1,   // 4 KiB granularity
        avl: 0,
        unusable: 0,
        padding: 0,
    }
}

fn make_data_segment() -> kvm_segment {
    kvm_segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector: DATA_SEL,
        // Type field: data(0), expand-up(0), writable(1), accessed(1) = 0x3
        type_: 0x3,
        present: 1,
        dpl: 0,
        db: 1, // 32-bit (conventional for data segments in long mode)
        s: 1,
        l: 0, // L bit only applies to code segments
        g: 1,
        avl: 0,
        unusable: 0,
        padding: 0,
    }
}

/// Configure the vCPU to enter 64-bit long mode at the kernel entry point.
///
/// This function:
/// 1. Writes the GDT into guest memory
/// 2. Writes page tables into guest memory
/// 3. Sets special registers (CR0, CR3, CR4, EFER, segment registers)
/// 4. Sets general registers (RIP, RSI, RFLAGS)
///
/// After this, the vCPU is ready to execute the first instruction of the
/// kernel in 64-bit mode. The kernel will set up its own GDT, page tables,
/// and stack almost immediately — our setup just needs to be valid long
/// enough for the kernel's `startup_64` code to take over.
pub fn configure(vcpu: &VcpuFd, guest_mem: &GuestMemoryMmap, entry_addr: u64) -> Result<()> {
    // --- Step 1: Write structures into guest memory ---

    write_gdt(guest_mem)?;
    write_page_tables(guest_mem)?;

    // --- Step 2: Special registers ---

    let mut sregs = vcpu.get_sregs().context("Failed to get sregs")?;

    // Control registers.
    sregs.cr0 = CR0_PE | CR0_MP | CR0_ET | CR0_NE | CR0_PG;
    sregs.cr3 = PML4_ADDR;
    sregs.cr4 = CR4_PAE;
    sregs.efer = EFER_LME | EFER_LMA;

    // GDT register. Points the CPU at our GDT in guest memory.
    // `base` is the guest physical address, `limit` is the size minus 1.
    sregs.gdt = kvm_bindings::kvm_dtable {
        base: GDT_ADDR,
        limit: 23, // 3 entries × 8 bytes - 1 = 23
        padding: [0; 3],
    };

    // Segment registers.
    sregs.cs = make_code_segment();
    sregs.ds = make_data_segment();
    sregs.es = make_data_segment();
    sregs.fs = make_data_segment();
    sregs.gs = make_data_segment();
    sregs.ss = make_data_segment();

    vcpu.set_sregs(&sregs).context("Failed to set sregs")?;

    // --- Step 3: General registers ---

    let regs = kvm_regs {
        // RIP: the kernel entry point. The vCPU starts executing here.
        rip: entry_addr,

        // RSI: pointer to struct boot_params. The kernel's startup_64
        // reads this to find the memory map and command line.
        rsi: BOOT_PARAMS_ADDR,

        // RFLAGS: bit 1 is reserved and must always be set to 1.
        // All other flags are 0 (interrupts disabled, direction flag clear).
        rflags: 0x2,

        // All other registers are zero. The kernel sets up its own stack
        // (RSP) and zeroes registers it cares about during startup.
        ..Default::default()
    };

    vcpu.set_regs(&regs).context("Failed to set regs")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BOOT_PARAMS_ADDR, CMDLINE_ADDR, CODE_SEL, CR0_PE, CR0_PG, CR4_PAE, DATA_SEL, E820_RAM,
        EFER_LME, EXT_RAM_START, GDT_ADDR, GDT_CODE, GDT_DATA, GDT_NULL, LOW_RAM_END, PD_ADDR,
        PDPT_ADDR, PML4_ADDR, PTE_PRESENT, PTE_PS, PTE_RW, configure, make_code_segment,
        make_data_segment, write_boot_params, write_cmdline, write_gdt, write_page_tables,
    };
    use linux_loader::bootparam::{boot_e820_entry, boot_params};
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    const TWO_MIB: u64 = 2 * 1024 * 1024;

    fn guest_mem(bytes: u64) -> GuestMemoryMmap {
        let len = usize::try_from(bytes).unwrap();
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), len)]).unwrap()
    }

    /// Guest address of a `boot_params` field, given its struct offset.
    fn boot_param_field(field_offset: usize) -> GuestAddress {
        GuestAddress(BOOT_PARAMS_ADDR + field_offset as u64)
    }

    // --- GDT descriptor encoding (Intel SDM Vol. 3A, 3.4.5 / 5.2.1) ---

    #[test]
    fn null_descriptor_is_zero() {
        assert_eq!(GDT_NULL, 0);
    }

    #[test]
    fn code_descriptor_is_64bit_ring0() {
        // Access byte (bits 47:40) = 0x9A: P=1, DPL=0, S=1, Type=code/exec/read.
        assert_eq!((GDT_CODE >> 40) & 0xFF, 0x9A);
        // Flags nibble (bits 55:52) = 0xA: G=1, D=0, L=1 -> 64-bit code segment.
        assert_eq!((GDT_CODE >> 52) & 0xF, 0xA);
    }

    #[test]
    fn data_descriptor_is_writable_ring0() {
        // Access byte = 0x92: P=1, DPL=0, S=1, Type=data/writable.
        assert_eq!((GDT_DATA >> 40) & 0xFF, 0x92);
        // Flags nibble = 0xC: G=1, D=1, L=0 (data segments are not long).
        assert_eq!((GDT_DATA >> 52) & 0xF, 0xC);
    }

    // --- write_gdt / write_page_tables / write_cmdline (guest memory writers) ---

    #[test]
    fn write_gdt_lays_out_null_code_data() {
        let mem = guest_mem(0x1000);
        write_gdt(&mem).unwrap();
        let null: u64 = mem.read_obj(GuestAddress(GDT_ADDR)).unwrap();
        let code: u64 = mem.read_obj(GuestAddress(GDT_ADDR + 8)).unwrap();
        let data: u64 = mem.read_obj(GuestAddress(GDT_ADDR + 16)).unwrap();
        assert_eq!(null, GDT_NULL);
        assert_eq!(code, GDT_CODE);
        assert_eq!(data, GDT_DATA);
    }

    #[test]
    fn page_tables_chain_pml4_pdpt_pd() {
        let mem = guest_mem(0x5000);
        write_page_tables(&mem).unwrap();
        let pml4: u64 = mem.read_obj(GuestAddress(PML4_ADDR)).unwrap();
        let pdpt: u64 = mem.read_obj(GuestAddress(PDPT_ADDR)).unwrap();
        assert_eq!(pml4, PDPT_ADDR | PTE_PRESENT | PTE_RW);
        assert_eq!(pdpt, PD_ADDR | PTE_PRESENT | PTE_RW);
        // Table pointers are not large-page entries.
        assert_eq!(pml4 & PTE_PS, 0);
    }

    #[test]
    fn page_directory_identity_maps_with_large_pages() {
        let mem = guest_mem(0x5000);
        write_page_tables(&mem).unwrap();
        for index in [0_u64, 1, 255, 511] {
            let entry: u64 = mem.read_obj(GuestAddress(PD_ADDR + index * 8)).unwrap();
            assert_ne!(entry & PTE_PRESENT, 0);
            assert_ne!(entry & PTE_RW, 0);
            assert_ne!(entry & PTE_PS, 0); // 2 MiB large page
            // Identity mapping: physical base (flags masked off) == index * 2 MiB.
            assert_eq!(entry & !0xFFF, index * TWO_MIB);
        }
    }

    #[test]
    fn write_cmdline_writes_string_then_null() {
        let mem = guest_mem(CMDLINE_ADDR + 0x1000);
        let cmdline = "console=ttyS0";
        write_cmdline(&mem, cmdline).unwrap();
        let mut buf = vec![0_u8; cmdline.len()];
        mem.read_slice(&mut buf, GuestAddress(CMDLINE_ADDR))
            .unwrap();
        assert_eq!(buf, cmdline.as_bytes());
        let terminator: u8 = mem
            .read_obj(GuestAddress(CMDLINE_ADDR + cmdline.len() as u64))
            .unwrap();
        assert_eq!(terminator, 0);
    }

    // --- write_boot_params (the e820 map — regression guard for the boot bug) ---

    #[test]
    fn boot_params_describe_two_ram_regions() {
        let mem = guest_mem(0x8000);
        let mem_size = 256 * TWO_MIB; // 512 MiB
        write_boot_params(&mem, "console=ttyS0", mem_size).unwrap();

        // The kernel discards e820 tables with fewer than two entries, so the
        // count must be exactly the two regions we wrote.
        let entries: u8 = mem
            .read_obj(boot_param_field(std::mem::offset_of!(
                boot_params,
                e820_entries
            )))
            .unwrap();
        assert_eq!(entries, 2);

        let table = std::mem::offset_of!(boot_params, e820_table);
        let stride = std::mem::size_of::<boot_e820_entry>();

        // Region 0: conventional memory [0, 640 KiB).
        let low_addr: u64 = mem.read_obj(boot_param_field(table)).unwrap();
        let low_size: u64 = mem.read_obj(boot_param_field(table + 8)).unwrap();
        let low_type: u32 = mem.read_obj(boot_param_field(table + 16)).unwrap();
        assert_eq!(low_addr, 0);
        assert_eq!(low_size, LOW_RAM_END);
        assert_eq!(low_type, E820_RAM);

        // Region 1: extended memory [1 MiB, top of RAM).
        let ext_addr: u64 = mem.read_obj(boot_param_field(table + stride)).unwrap();
        let ext_size: u64 = mem.read_obj(boot_param_field(table + stride + 8)).unwrap();
        assert_eq!(ext_addr, EXT_RAM_START);
        assert_eq!(ext_size, mem_size - EXT_RAM_START);
    }

    #[test]
    fn boot_params_record_cmdline_pointer_and_loader_type() {
        let mem = guest_mem(0x8000);
        write_boot_params(&mem, "x", 64 * TWO_MIB).unwrap();

        let cmd_ptr: u32 = mem
            .read_obj(boot_param_field(std::mem::offset_of!(
                boot_params,
                hdr.cmd_line_ptr
            )))
            .unwrap();
        assert_eq!(u64::from(cmd_ptr), CMDLINE_ADDR);

        let loader: u8 = mem
            .read_obj(boot_param_field(std::mem::offset_of!(
                boot_params,
                hdr.type_of_loader
            )))
            .unwrap();
        assert_eq!(loader, 0xFF);
    }

    // --- segment register builders ---

    #[test]
    fn code_segment_is_long_mode() {
        let cs = make_code_segment();
        assert_eq!(cs.selector, CODE_SEL);
        assert_eq!(cs.l, 1); // 64-bit
        assert_eq!(cs.db, 0); // must be 0 when L=1
        assert_eq!(cs.present, 1);
        assert_eq!(cs.dpl, 0);
    }

    #[test]
    fn data_segment_is_present_and_not_long() {
        let ds = make_data_segment();
        assert_eq!(ds.selector, DATA_SEL);
        assert_eq!(ds.l, 0);
        assert_eq!(ds.present, 1);
    }

    // --- configure (requires /dev/kvm; skips cleanly when unavailable) ---

    #[test]
    fn configure_puts_vcpu_in_long_mode() {
        let Ok(kvm) = kvm_ioctls::Kvm::new() else {
            eprintln!("skipping configure test: /dev/kvm not accessible");
            return;
        };
        let vm = kvm.create_vm().unwrap();
        let mem = guest_mem(TWO_MIB); // covers GDT + page tables
        let vcpu = vm.create_vcpu(0).unwrap();
        let entry = 0x10_0000;

        configure(&vcpu, &mem, entry).unwrap();

        let sregs = vcpu.get_sregs().unwrap();
        assert_eq!(sregs.cr3, PML4_ADDR);
        assert_ne!(sregs.cr0 & CR0_PE, 0);
        assert_ne!(sregs.cr0 & CR0_PG, 0);
        assert_ne!(sregs.cr4 & CR4_PAE, 0);
        assert_ne!(sregs.efer & EFER_LME, 0);
        assert_eq!(sregs.cs.selector, CODE_SEL);
        assert_eq!(sregs.cs.l, 1);

        let regs = vcpu.get_regs().unwrap();
        assert_eq!(regs.rip, entry);
        assert_eq!(regs.rsi, BOOT_PARAMS_ADDR);
    }
}
