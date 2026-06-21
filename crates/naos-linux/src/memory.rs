// memory.rs
//
// Guest memory allocation and KVM registration.
//
// This module does two things:
// 1. Allocates host memory via mmap (wrapped by vm-memory's GuestMemoryMmap)
// 2. Registers that memory with KVM so the guest can access it
//
// After this module runs, the guest has a contiguous block of RAM starting
// at physical address 0. Everything else in the VMM (kernel loading, page
// tables, boot params) writes into this memory.

use anyhow::{Context, Result};
use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use vm_memory::{Address, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

/// Build the guest's physical memory.
///
/// Creates a single anonymous mmap region of `size_mib` MiB at guest
/// physical address 0. Returns a `GuestMemoryMmap` that provides typed,
/// bounds-checked access to the region.
///
/// We use a single contiguous region with no gaps. Real VMMs often have
/// an MMIO hole (typically at 3-4 GiB) where device registers live, but
/// we have no MMIO devices in the MVP, so the address space is pure RAM.
pub fn build(size_mib: u64) -> Result<GuestMemoryMmap> {
    let size_bytes = size_mib
        .checked_mul(1024 * 1024)
        .context("Memory size overflow")?;
    // from_ranges wants a usize length. On 64-bit hosts this is a no-op; on a
    // 32-bit host try_from rejects a guest larger than the host address space
    // instead of silently truncating.
    let size_bytes = usize::try_from(size_bytes).context("Memory size exceeds usize")?;

    // GuestMemoryMmap::from_ranges takes a slice of (GuestAddress, usize) pairs.
    // Each pair defines one region: starting guest physical address and size.
    // Internally, this calls mmap(2) with MAP_ANONYMOUS | MAP_PRIVATE to allocate
    // the backing memory. The kernel zero-fills it (MAP_ANONYMOUS guarantees this),
    // so the guest sees zeroed RAM — same as real hardware on power-on.
    GuestMemoryMmap::from_ranges(&[(GuestAddress(0), size_bytes)])
        .context("Failed to create guest memory via mmap")
}

/// Register guest memory with KVM.
///
/// This tells KVM: "when the guest accesses physical address X, map it to
/// host virtual address Y." KVM programs the CPU's hardware page tables
/// (Extended Page Tables on Intel, Nested Page Tables on AMD) to enforce
/// this mapping. After this call, guest memory accesses go through hardware
/// translation with no VMM involvement — we don't get a vmexit on every
/// memory access, only on accesses to addresses outside registered regions
/// (which would be MMIO, but we don't have any).
///
/// The `unsafe` here is because we're asserting to KVM that the memory
/// region is valid and will remain valid for the lifetime of the VM.
/// `GuestMemoryMmap`'s mmap is anonymous and owned by our process, so this
/// holds as long as we don't drop `guest_mem` before the VM.
pub fn register(vm: &VmFd, guest_mem: &GuestMemoryMmap) -> Result<()> {
    // We have exactly one region. Iterate over it to get the host address
    // and guest address, which we need for the KVM ioctl.
    let region = guest_mem
        .iter()
        .next()
        .context("Guest memory has no regions")?;

    let mem_region = kvm_userspace_memory_region {
        // Slot 0. KVM uses slot indices to track memory regions. We have
        // one region, so slot 0 is all we need. If we later add an MMIO
        // hole, we'd split into slot 0 (below the hole) and slot 1 (above).
        slot: 0,
        // No flags. KVM_MEM_READONLY would make this ROM; we want normal RAM.
        flags: 0,
        // Guest physical address where this region starts.
        guest_phys_addr: region.start_addr().raw_value(),
        // Size in bytes.
        memory_size: region.len(),
        // Host virtual address of the mmap'd backing memory.
        // This is the pointer KVM will use to resolve guest physical addresses.
        userspace_addr: region.as_ptr() as u64,
    };

    // SAFETY: The memory region is valid (mmap succeeded), properly aligned,
    // and will outlive the VM because GuestMemoryMmap is owned by the Vmm
    // struct alongside the VmFd.
    unsafe { vm.set_user_memory_region(mem_region) }
        .context("KVM_SET_USER_MEMORY_REGION failed")?;

    Ok(())
}
