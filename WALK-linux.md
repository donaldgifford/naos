# naos-linux: a first-principles walkthrough

This document walks through every file in `naos-linux` — a minimum viable
KVM-based hypervisor that boots an x86_64 Linux kernel to dmesg and exits
cleanly. Every constant is traced back to its specification, every design choice
is annotated, and every file is presented in dependency order so you can build
understanding from the ground up.

**What we're building:** a ~350-line Rust program that opens `/dev/kvm`, creates
a virtual machine, loads a Linux kernel into guest memory, configures a vCPU to
enter 64-bit long mode at the kernel's entry point, emulates a single 16550 UART
so the kernel can print to our terminal, and runs until the kernel panics
because there is no init process.

**What success looks like:**

```
[    0.000000] Linux version 6.12.0 ...
[    0.000000] Command line: console=ttyS0 reboot=k panic=1 pci=off
...
[    0.123456] Kernel panic - not syncing: No working init found.
```

That panic is the win condition. It means the kernel booted, initialized its
subsystems, and got far enough to look for userspace. Everything before that
panic exercised every piece of our hypervisor: memory mapping, kernel loading,
CPU state setup, and serial I/O.

---

## Table of contents

1. [Architecture overview](#1-architecture-overview)
2. [The guest physical memory map](#2-the-guest-physical-memory-map)
3. [Dependencies](#3-dependencies)
4. [memory.rs — guest memory](#4-memoryrs--guest-memory)
5. [kernel.rs — loading the kernel](#5-kernelrs--loading-the-kernel)
6. [boot.rs — the boot environment](#6-bootrs--the-boot-environment)
7. [serial.rs — UART emulation](#7-serialrs--uart-emulation)
8. [vcpu.rs — the run loop](#8-vcpurs--the-run-loop)
9. [vmm.rs — tying it together](#9-vmmrs--tying-it-together)
10. [main.rs — entry point](#10-mainrs--entry-point)
11. [Running it](#11-running-it)
12. [What happens during boot](#12-what-happens-during-boot)
13. [What's next](#13-whats-next)

---

## 1. Architecture overview

A KVM-based VMM is a normal Linux userspace process that asks the kernel to run
code inside a hardware-isolated virtual machine. The relationship is:

```
┌──────────────────────────────────────────────────────────┐
│  Host Linux kernel                                       │
│  ┌──────────────────────────────────────────────────────┐│
│  │  KVM module (/dev/kvm)                               ││
│  │                                                      ││
│  │  Manages:                                            ││
│  │  - VM file descriptors (guest containers)            ││
│  │  - vCPU file descriptors (virtual processors)        ││
│  │  - Memory slot mappings (guest ↔ host address space) ││
│  │  - In-kernel device emulation (PIC, IOAPIC, PIT)     ││
│  │                                                      ││
│  │  Executes guest code via VMLAUNCH/VMRESUME (Intel)   ││
│  │  or VMRUN (AMD) — hardware-level isolation           ││
│  └──────────────────────────────────────────────────────┘│
│                         ▲                                │
│                         │ ioctl() syscalls               │
│                         │                                │
│  ┌──────────────────────┴───────────────────────────────┐│
│  │  naos-linux (our userspace VMM)                      ││
│  │                                                      ││
│  │  Responsibilities:                                   ││
│  │  1. Allocate guest memory (mmap)                     ││
│  │  2. Load the kernel into guest memory                ││
│  │  3. Configure vCPU registers for long mode entry     ││
│  │  4. Run the vCPU (KVM_RUN ioctl in a loop)           ││
│  │  5. Handle VM exits (I/O to serial port)             ││
│  │  6. Emulate devices (16550 UART → stdout)            ││
│  └──────────────────────────────────────────────────────┘│
└──────────────────────────────────────────────────────────┘
```

The division of labor: KVM handles the heavy lifting of CPU virtualization
(entering and exiting guest mode, memory isolation via EPT/NPT, interrupt
injection). We handle everything else — setting up the environment the guest
expects, and emulating any device the guest tries to talk to.

This is the same architecture Firecracker, Cloud Hypervisor, and crosvm use. The
difference is scope: they emulate dozens of devices and handle hundreds of edge
cases. We emulate one device and handle three exit reasons.

### The KVM API in 60 seconds

KVM exposes three layers of file descriptors, each opened via `ioctl()` on the
previous:

```
/dev/kvm                    ← system-level (check version, get capabilities)
  └─ ioctl(KVM_CREATE_VM)
      → VM fd               ← per-VM (memory slots, IRQ chip, create vCPUs)
        └─ ioctl(KVM_CREATE_VCPU)
            → vCPU fd       ← per-vCPU (set registers, run guest code)
```

The entire KVM API is ioctls on these three file descriptors. `kvm-ioctls` wraps
them in Rust structs (`Kvm`, `VmFd`, `VcpuFd`), but underneath it's all
`ioctl(2)`. When something goes wrong, `strace -e ioctl` is the gold standard
for debugging because it shows you exactly what naos is asking the kernel to do.

> **Spec reference:** The KVM API is documented in
> [Documentation/virt/kvm/api.rst](https://docs.kernel.org/virt/kvm/api.html) in
> the Linux kernel tree. It is the authoritative reference for every ioctl
> mentioned in this walkthrough.

---

## 2. The guest physical memory map

Before writing any code, we need to decide where things go in the guest's
physical address space. This is an x86_64 machine we're building, and the kernel
has expectations about what it will find at certain addresses.

```
Guest Physical Address Space (256 MiB)
═══════════════════════════════════════════════════════

0x0000_0000  ┌──────────────────────────────────────┐
             │  Real-mode IVT / BIOS data area      │ ← we don't use this,
             │  (legacy, not relevant for 64-bit)   │   but avoid clobbering
0x0000_0500  ├──────────────────────────────────────┤
             │  GDT (3 entries × 8 bytes = 24 bytes)│ ← our global descriptor table
0x0000_0520  ├──────────────────────────────────────┤
             │  (unused gap)                        │
0x0000_1000  ├──────────────────────────────────────┤
             │  PML4 (4096 bytes, page-aligned)     │ ← level 4 page table
0x0000_2000  ├──────────────────────────────────────┤
             │  PDPT (4096 bytes, page-aligned)     │ ← level 3 page table
0x0000_3000  ├──────────────────────────────────────┤
             │  PD   (4096 bytes, page-aligned)     │ ← level 2 page table (2 MiB pages)
0x0000_4000  ├──────────────────────────────────────┤
             │  (unused gap)                        │
0x0000_7000  ├──────────────────────────────────────┤
             │  Boot parameters / "zero page"       │ ← struct boot_params (4096 bytes)
             │  (e820 map, cmdline ptr, etc.)       │
0x0000_8000  ├──────────────────────────────────────┤
             │  (unused gap)                        │
0x0002_0000  ├──────────────────────────────────────┤
             │  Kernel command line string          │ ← "console=ttyS0 reboot=k ..."
             │  (up to 4096 bytes, null-terminated) │
0x0002_1000  ├──────────────────────────────────────┤
             │  (unused gap)                        │
0x0100_0000  ├──────────────────────────────────────┤  ← 16 MiB
             │  Kernel (vmlinux ELF segments)       │ ← loaded by linux-loader at
             │  .text, .rodata, .data, .bss, ...    │   the addresses the ELF specifies
             │  ...                                 │   (typically starting around here)
             ├──────────────────────────────────────┤
             │  (rest of RAM, unused for MVP)       │
0x1000_0000  └──────────────────────────────────────┘  ← 256 MiB

```

**Why these specific addresses?**

- **0x500 for the GDT:** The first 0x500 bytes are the real-mode interrupt
  vector table and BIOS data area. We're not using real mode, but this region is
  traditionally avoided. 0x500 is the first "safe" low address and is what
  Firecracker uses.
- **0x1000, 0x2000, 0x3000 for page tables:** Page tables must be 4096-byte
  aligned (the bottom 12 bits of CR3 and each table entry are used for flags,
  not the address). These three consecutive pages give us PML4, PDPT, and PD
  without gaps.
- **0x7000 for boot parameters:** Convention from the Linux boot protocol. The
  "zero page" (struct boot_params) lives here. The kernel looks at RSI to find
  this structure.
- **0x20000 for the cmdline:** Needs to be in low memory, reachable by 32-bit
  pointers (the cmdline_ptr field in boot_params is u32). 0x20000 = 128 KiB,
  well within range.
- **0x1000000+ for the kernel:** The vmlinux ELF's program headers specify where
  each segment loads. Typical `tinyconfig` kernels load starting at 0x1000000
  (16 MiB). We don't choose this — the ELF loader follows the headers.

We encode these as constants:

```rust
// guest_addr.rs or at the top of boot.rs — all guest physical addresses
// that we manually place things at. The kernel's load address is not here
// because the ELF loader determines it from the binary's headers.

/// Global Descriptor Table. Three 8-byte entries starting here.
/// Placed at the first usable address after the legacy BIOS data area.
const GDT_ADDR: u64 = 0x500;

/// PML4 page table (level 4). Must be 4096-byte aligned.
const PML4_ADDR: u64 = 0x1000;

/// Page Directory Pointer Table (level 3). Must be 4096-byte aligned.
const PDPT_ADDR: u64 = 0x2000;

/// Page Directory (level 2). Uses 2 MiB large pages. Must be 4096-byte aligned.
const PD_ADDR: u64 = 0x3000;

/// struct boot_params (the "zero page"). 4096 bytes.
/// Contains the e820 memory map, cmdline pointer, and boot protocol header.
const BOOT_PARAMS_ADDR: u64 = 0x7000;

/// Kernel command line string. Null-terminated, up to 4096 bytes.
const CMDLINE_ADDR: u64 = 0x2_0000;
```

---

## 3. Dependencies

The full `Cargo.toml` for `crates/naos-linux`:

```toml
[package]
name = "naos-linux"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
publish = false

[dependencies]
# KVM API wrappers. kvm-ioctls provides the Kvm/VmFd/VcpuFd types that
# map to /dev/kvm ioctls. kvm-bindings provides the raw kernel structs
# (kvm_regs, kvm_sregs, kvm_segment, kvm_userspace_memory_region, etc.)
# generated via bindgen from linux/kvm.h.
kvm-ioctls = "0.25"
kvm-bindings = { version = "0.14", features = ["fam-wrappers"] }

# Guest memory abstraction. GuestMemoryMmap gives us a typed, bounds-checked
# view over an mmap'd region that we register with KVM as guest RAM.
#
# Pinned to 0.17 to match the version linux-loader depends on. If these
# diverge, two copies of vm-memory end up in the tree, GuestMemoryMmap
# resolves to two distinct types, and linux-loader's `M: GuestMemory` bound
# stops accepting our guest memory — you get cryptic "trait not satisfied" /
# "mismatched types" errors that point back into vm-memory.
vm-memory = { version = "0.17", features = ["backend-mmap"] }

# Linux kernel loader. Parses vmlinux ELF binaries and loads their segments
# into guest memory. Also provides the boot_params struct definition.
linux-loader = { version = "0.13", features = ["elf", "bzimage"] }

# 16550 UART emulation. Pure-logic state machine that handles reads/writes
# to the eight UART registers. We wire its output to stdout.
vm-superio = "0.8"

# Linux utility types. EventFd (wrapped as the serial interrupt trigger) and
# other small wrappers around Linux-specific primitives.
vmm-sys-util = "0.15"

# Raw libc bindings. We need exactly one symbol — EFD_NONBLOCK, the
# eventfd(2) flag passed when creating the serial interrupt fd.
libc = "0.2"

# Error handling. anyhow gives us context-rich error chains that propagate
# cleanly from any fallible operation to main's error handler.
anyhow = "1"

# CLI argument parsing. Three arguments, no subcommands, minimal setup.
# The `derive` feature is mandatory — without it, clap exposes only its
# runtime builder API and the #[derive(Parser)] macro does not exist.
clap = { version = "4", features = ["derive"] }

[lints]
workspace = true
```

> **Version note:** The exact versions above may drift as crates are updated.
> When you start coding, run `cargo add <crate>` to pull the latest compatible
> version rather than copying these verbatim. The important thing is the set of
> crates and _why_ each is included, not the pinned version.

**Why not more?** Every crate not in this list was considered and rejected:

- `event-manager` — we have one vCPU on one thread doing blocking I/O. An event
  loop adds complexity for zero benefit until we have a second device or second
  thread.
- `vm-allocator` — we have six hardcoded addresses. An allocator is overhead
  until we have dynamic device placement.
- `vm-device` — a device trait abstraction is premature when we have exactly one
  device (the UART).
- All `virtio-*` crates — virtio requires an MMIO bus, IRQ routing, and queue
  management. That's MVP+1, not MVP.

---

## 4. memory.rs — guest memory

### What this file does

Allocates a chunk of host memory via `mmap(2)`, then tells KVM "this host memory
region represents guest physical addresses 0 through N." After this, when the
guest CPU accesses a physical address, KVM's hardware page tables (EPT on Intel,
NPT on AMD) translate it to the corresponding offset in our mmap'd region. The
guest thinks it has real RAM; the host knows it's a memory-mapped region in our
process.

### How KVM memory works

```
Host process address space              Guest physical address space
┌─────────────────────────┐             ┌─────────────────────────┐
│                         │             │                         │
│  mmap'd region ─────────┼─────────────┼─► 0x0000_0000           │
│  (256 MiB of anon mem)  │  KVM maps   │   Guest RAM             │
│  host_addr: 0x7f...     │  these via  │   (256 MiB)             │
│  length: 0x1000_0000    │  EPT / NPT  │                         │
│                         │             │                         │
│                         │             ├─► 0x1000_0000           │
└─────────────────────────┘             │   End of guest RAM      │
                                        └─────────────────────────┘
```

KVM's `KVM_SET_USER_MEMORY_REGION` ioctl creates this mapping. The struct it
takes:

```
kvm_userspace_memory_region {
    slot:            u32,  // an index (we use 0, the first and only slot)
    flags:           u32,  // 0 for normal RAM, KVM_MEM_READONLY for ROM
    guest_phys_addr: u64,  // where this region appears in the guest (0x0)
    memory_size:     u64,  // size in bytes (256 * 1024 * 1024)
    userspace_addr:  u64,  // host virtual address of the mmap'd region
}
```

> **Spec reference:** `KVM_SET_USER_MEMORY_REGION` is documented in
> [KVM API docs, section 4.35](https://docs.kernel.org/virt/kvm/api.html#kvm-set-user-memory-region).

### The code

```rust
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
```

### What can go wrong

- **`mmap` fails** — usually because the host doesn't have enough virtual
  address space (unlikely with 256 MiB) or because the kernel enforces
  `vm.max_map_count` limits. The error message from
  `GuestMemoryMmap::from_ranges` will tell you.
- **`set_user_memory_region` fails** — usually because the VM fd is invalid (we
  called things in the wrong order) or because the host address is not
  page-aligned (mmap guarantees this, so this shouldn't happen through
  `GuestMemoryMmap`).

---

## 5. kernel.rs — loading the kernel

### What vmlinux is

Linux kernels come in several formats:

- **bzImage** — the compressed, bootable image most distros ship. It includes a
  real-mode setup header and a decompressor stub. Loading it requires emulating
  the real-mode → protected-mode → long-mode transition, which is complex.
- **vmlinux** — the raw, uncompressed ELF binary. This is what the kernel build
  system produces _before_ compression. It can be loaded directly into memory at
  the addresses its ELF program headers specify, and entered at its ELF entry
  point in 64-bit mode. No decompressor, no real-mode stub, no boot protocol
  complexity.

We use vmlinux because it avoids the entire real-mode boot path. The tradeoff is
that vmlinux files are larger (10-20 MiB for a tinyconfig kernel instead of 2-3
MiB for a bzImage), but for a learning project, simplicity wins over file size.

### How ELF loading works

An ELF binary contains program headers that say "load these bytes from file
offset X into memory address Y." For a vmlinux kernel, the addresses are guest
physical addresses (the kernel is linked to run at specific physical addresses).
The `linux-loader` crate reads these headers and copies the segments into our
guest memory.

```
vmlinux ELF file                    Guest physical memory
┌─────────────────────┐             ┌──────────────────────────┐
│  ELF header         │             │                          │
│  e_entry: 0x1000000 │─────────┐   │                          │
│                     │         │   │                          │
│  Program header 0   │         │   │                          │
│  p_vaddr: 0x1000000 │──────┐  │   │                          │
│  p_filesz: 0x300000 │      │  │   ├──────────────────────────┤
│  p_memsz:  0x400000 │      ├──┼──►│  .text + .rodata         │ 0x1000000
│                     │      │  │   │  (code + read-only data) │
│  Segment 0 bytes    │──────┘  │   │                          │
│  (the actual code)  │         │   │  .data + .bss            │
│                     │         │   │  (initialized + zeroed)  │
└─────────────────────┘         │   ├──────────────────────────┤
                                │   │                          │
                                │   │                          │
                         RIP ◄──┘   └──────────────────────────┘

                    Entry point = e_entry from ELF header
                    This is where we set the vCPU's RIP register.
```

> **Spec reference:** The ELF format is defined in the
> [System V ABI specification](https://refspecs.linuxfoundation.org/elf/elf.pdf),
> chapters 4-5. For our purposes, only `PT_LOAD` segments matter.

### The code

```rust
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
```

### Why this is only 20 lines

Because `linux-loader` handles all the ELF complexity: parsing the header,
iterating program headers, copying segment bytes, zeroing .bss, validating that
the kernel fits in guest memory. Our job is just to open the file and call it.

We could do this ourselves — ELF parsing is not magical — but it's exactly the
kind of "solved problem with no learning value in reimplementing" that the
first-principles philosophy says to use a crate for. The learning value is in
_understanding_ ELF, which this walkthrough gives you, not in debugging an ELF
parser.

---

## 6. boot.rs — the boot environment

This is the longest and most important file. It does four things:

1. **Build boot parameters** — the "zero page" (struct boot_params) that tells
   the kernel about its memory map and command line
2. **Write the GDT** — the global descriptor table that x86_64 requires for
   segmentation
3. **Build page tables** — identity-mapped 4-level paging for the first 1 GiB
4. **Configure CPU registers** — special registers (CR0, CR3, CR4, EFER) and
   general registers (RIP, RSI, RFLAGS) to enter 64-bit long mode

Each of these is a sub-section with its own explanation and diagram.

### Part 1: boot parameters (the "zero page")

When the Linux kernel starts in 64-bit mode, it expects register RSI to point to
a `struct boot_params` in memory. This 4096-byte structure (historically called
the "zero page" because it was at physical address 0 in early Linux) contains
the information a bootloader would normally provide: how much memory the machine
has, where the kernel command line is, and various boot protocol fields.

We need to fill in three things:

1. **The e820 memory map** — tells the kernel what ranges of physical addresses
   are usable RAM. Without this, the kernel doesn't know how much memory it has
   and will panic early.
2. **The command line pointer** — tells the kernel where to find the boot
   command line (e.g., `console=ttyS0`). Without this, serial output won't be
   configured.
3. **The boot protocol header** — a few fields that identify us as a boot loader
   and specify the protocol version.

```
struct boot_params (4096 bytes at BOOT_PARAMS_ADDR = 0x7000)
┌──────────────────────────────────────────────────────────┐
│  ...                                                     │
│  Offset 0x1F1: struct setup_header                       │
│    ...                                                   │
│    Offset 0x228: cmd_line_ptr (u32) ──────► 0x20000      │
│    Offset 0x238: cmdline_size (u32) = length of cmdline  │
│    ...                                                   │
│  Offset 0x1E8: e820_entries (u8) = 1                     │
│  ...                                                     │
│  Offset 0x2D0: e820_table[0]                             │
│    addr: 0x0000_0000_0000_0000                           │
│    size: 0x0000_0000_1000_0000  (256 MiB)                │
│    type: 1  (E820_RAM)                                   │
│  ...                                                     │
└──────────────────────────────────────────────────────────┘
```

> **Spec reference:** `struct boot_params` is defined in
> [arch/x86/include/uapi/asm/bootparam.h](https://github.com/torvalds/linux/blob/master/arch/x86/include/uapi/asm/bootparam.h)
> in the kernel source. The e820 memory map format is documented in
> [Documentation/arch/x86/boot.rst](https://www.kernel.org/doc/html/latest/arch/x86/boot.html).

### Part 2: the GDT

x86_64 famously "got rid of segmentation" — except it didn't, not entirely. The
CPU still requires a Global Descriptor Table with at least a valid code segment
and data segment, even in 64-bit long mode where segment bases and limits are
ignored. The GDT is a vestige of the 16-bit/32-bit era that the architecture
can't fully shed.

Our GDT has three entries:

```
Entry 0 (offset 0x00): Null descriptor — required by the CPU, must be all zeros
Entry 1 (offset 0x08): 64-bit code segment — selector 0x08, used by CS
Entry 2 (offset 0x10): Data segment — selector 0x10, used by DS/ES/FS/GS/SS
```

Each entry is 8 bytes. The GDT occupies 24 bytes starting at guest physical
address 0x500.

#### GDT entry format (8 bytes)

Every GDT entry is a segment descriptor with this bit layout:

```
 63       56 55  52 51  48 47       40 39       32
┌──────────┬──────┬──────┬──────────┬──────────┐
│ Base     │Flags │Limit │ Access   │ Base     │
│ 31:24    │      │19:16 │ Byte     │ 23:16    │
│ (8 bits) │      │(4 b) │ (8 bits) │ (8 bits) │
└──────────┴──────┴──────┴──────────┴──────────┘
 31                 16 15                      0
┌─────────────────────┬────────────────────────┐
│ Base 15:0           │ Limit 15:0             │
│ (16 bits)           │ (16 bits)              │
└─────────────────────┴────────────────────────┘

Flags (bits 55:52):
  Bit 55: G  (Granularity: 0 = byte, 1 = 4 KiB pages)
  Bit 54: D/B (Default size: 0 = 16-bit, 1 = 32-bit; MUST be 0 for 64-bit code)
  Bit 53: L  (Long mode: 1 = 64-bit code segment)
  Bit 52: AVL (Available for system software, unused by CPU)

Access byte (bits 47:40):
  Bit 47: P   (Present: must be 1 for valid segments)
  Bits 46:45: DPL  (Privilege level: 0 = ring 0 / kernel)
  Bit 44: S   (Descriptor type: 1 = code/data, 0 = system)
  Bits 43:40: Type (segment type, meaning depends on code vs data)
    Code: 43=1 (code), 42=C (conforming), 41=R (readable), 40=A (accessed)
    Data: 43=0 (data), 42=E (expand-down), 41=W (writable), 40=A (accessed)
```

> **Spec reference:** GDT entry format is defined in the Intel SDM Vol. 3A,
> Section 3.4.5 ("Segment Descriptors") or AMD APM Vol. 2, Section 4.7 ("Legacy
> Segment Descriptors").

#### Our three entries, byte by byte

**Null descriptor (entry 0):** All zeros. Required by the CPU.

```rust
const GDT_NULL: u64 = 0;
```

**64-bit code segment (entry 1):**

```
Base  = 0x00000000  (ignored in long mode, but must be valid)
Limit = 0xFFFFF     (ignored in long mode with G=1)
Access byte = 0x9A:
  P=1, DPL=00, S=1, Type=1010 (code, non-conforming, readable, not accessed)
  Binary: 1_00_1_1010 = 0x9A
Flags = 0xA:
  G=1, D=0 (MUST be 0 for 64-bit), L=1 (64-bit mode), AVL=0
  Binary: 1_0_1_0 = 0xA

Combined as u64: 0x00AF_9A00_0000_FFFF
```

```rust
const GDT_CODE: u64 = 0x00AF_9A00_0000_FFFF;
```

**Data segment (entry 2):**

```
Base  = 0x00000000
Limit = 0xFFFFF
Access byte = 0x92:
  P=1, DPL=00, S=1, Type=0010 (data, expand-up, writable, not accessed)
  Binary: 1_00_1_0010 = 0x92
Flags = 0xC:
  G=1, D=1 (32-bit), L=0 (not a code segment), AVL=0
  Binary: 1_1_0_0 = 0xC

Combined as u64: 0x00CF_9200_0000_FFFF
```

```rust
const GDT_DATA: u64 = 0x00CF_9200_0000_FFFF;
```

> **Why D=0 for code but D=1 for data?** The Intel SDM (Vol. 3A, Section 5.2.1)
> mandates that when L=1 (64-bit code segment), D must be 0, or the CPU raises a
> general protection fault. Data segments don't use the L bit, so D=1 is fine
> and means "32-bit default operand size," which is conventional even though it
> doesn't affect behavior in long mode.

### Part 3: page tables

x86*64 uses 4-level page tables to translate virtual addresses to physical
addresses. In long mode, paging is \_mandatory* — you cannot have CR0.PG=1
(paging enabled, required for long mode) without valid page tables.

We build the simplest possible page tables: identity-mapped (virtual address =
physical address) using 2 MiB large pages, covering the first 1 GiB of address
space. This means guest virtual address 0x1234 maps to guest physical address
0x1234. The kernel will replace these with its own page tables during boot, but
it needs _something_ valid to start with.

#### The 4-level page table walk

When the CPU translates a 48-bit virtual address, it walks four levels of
tables:

```
Virtual address (48 bits used):
┌────────┬────────┬────────┬────────┬──────────────┐
│ PML4   │ PDPT   │  PD    │  PT    │ Page offset  │
│ index  │ index  │ index  │ index  │              │
│ 47:39  │ 38:30  │ 29:21  │ 20:12  │ 11:0         │
│ 9 bits │ 9 bits │ 9 bits │ 9 bits │ 12 bits      │
└───┬────┴───┬────┴───┬────┴───┬────┴──────────────┘
    │        │        │        │
    ▼        │        │        │
┌────────┐   │        │        │
│ PML4   │   │        │        │
│ table  │   │        │        │
│(512 ent│   │        │        │
│ at CR3)│   │        │        │
│ [idx]──┼───┼─►┌────────┐     │
└────────┘   │  │ PDPT   │     │
             │  │ table  │     │
             │  │(512 ent│     │
             │  │ [idx]──┼─────┼►┌────────┐
             │  └────────┘     │ │ PD     │
             │                 │ │ table  │
             │                 │ │(512 ent│
             │                 │ │ [idx]──┼──► Physical page (2 MiB)
             │                 │ └────────┘    (when PS bit = 1)
             │                 │
             │                 │ If PS=0, walk continues to PT level
             │                 │ (we don't use this — 2 MiB pages only)
             │                 │
```

**With 2 MiB large pages (PS bit set in PD entry), we skip the PT level
entirely.** The PD entry directly maps a 2 MiB physical page. This means:

- We need one PML4 (512 entries, but we only fill entry 0)
- We need one PDPT (512 entries, but we only fill entry 0)
- We need one PD (512 entries × 2 MiB per entry = 1 GiB coverage)

Total: 3 pages = 12 KiB of page tables. That covers the first 1 GiB of address
space, which is more than enough for a 256 MiB guest.

#### Page table entry format

Each entry in a page table is 8 bytes (64 bits):

```
 63  62:52  51:12                           11:0
┌───┬──────┬─────────────────────────────┬──────────────┐
│NX │ Avl  │ Physical address of next    │ Flags        │
│   │      │ table or page (bits 51:12)  │              │
└───┴──────┴─────────────────────────────┴──────────────┘

Key flags (bits 11:0):
  Bit 0: P   (Present — must be 1 for valid entries)
  Bit 1: R/W (Read/Write — 1 = writable)
  Bit 2: U/S (User/Supervisor — 0 = kernel only)
  Bit 7: PS  (Page Size — 1 = large page; only valid in PD entries)
```

For our identity mapping:

```
PML4[0] = PDPT_ADDR | P | R/W    = 0x2000 | 0x3 = 0x2003
PDPT[0] = PD_ADDR   | P | R/W    = 0x3000 | 0x3 = 0x3003
PD[i]   = (i * 2MB)  | P | R/W | PS = (i * 0x200000) | 0x83
```

> **Spec reference:** 4-level paging is defined in Intel SDM Vol. 3A, Section
> 4.5 ("4-Level Paging and 5-Level Paging") or AMD APM Vol. 2, Section 5.3
> ("Long-Mode Page Translation").

### Part 4: CPU registers

After the GDT and page tables are written into guest memory, we configure the
vCPU's registers via KVM's `KVM_SET_SREGS` and `KVM_SET_REGS` ioctls. This is
how we tell the CPU "you are in 64-bit long mode, your page tables are at
address X, your GDT is at address Y, and you should start executing at address
Z."

#### Control registers

```
CR0 (Control Register 0):
┌────┬──┬──┬──┬──┬──┬──┬───────────────────────────────┬──┐
│ PG │  │  │  │  │NE│ET│              ...              │PE│
│ b31│  │  │  │  │b5│b4│                               │b0│
└──┬─┴──┴──┴──┴──┴─┬┴─┬┴───────────────────────────────┴─┬┘
   │               │  │                                  │
   │ PG = 1: Enable paging (required for long mode)      │
   │ NE = 1: Numeric error reporting via #MF, not IRQ 13 │
   │ ET = 1: Extension type (hardwired to 1 on modern CPUs)
   PE = 1: Protection enable (required for long mode)

   CR0 = 0x8000_0033  (PE | ET | NE | PG, plus MP bit which is conventional)

CR3 (Page Table Base Register):
   Points to the physical address of the PML4.
   CR3 = PML4_ADDR = 0x1000

CR4 (Control Register 4):
   PAE (bit 5) = 1: Physical Address Extension.
   Required for long mode — 4-level paging is a PAE feature.
   CR4 = 0x20

EFER (Extended Feature Enable Register, MSR 0xC0000080):
   LME (bit 8) = 1: Long Mode Enable
   LMA (bit 10) = 1: Long Mode Active
   EFER = 0x500
```

> **Spec reference:** CR0 bits are defined in Intel SDM Vol. 3A, Section 2.5
> ("Control Registers") or AMD APM Vol. 2, Section 3.1.1. EFER is in Intel SDM
> Vol. 3A, Section 2.2.1 or AMD APM Vol. 2, Section 3.1.7.

> **Why set both LME and LMA?** LME is the "enable" switch; LMA is the "active"
> indicator. On real hardware, the CPU sets LMA automatically when you enable
> paging with LME=1 and CR0.PG=1. But we're setting registers via KVM before the
> vCPU has run a single instruction, so we set both to tell KVM "this vCPU
> should start in active long mode."

#### Segment registers

Even in 64-bit mode, the CPU uses segment registers. CS determines the current
privilege level and code segment attributes. DS/ES/FS/GS/SS are largely ignored
in long mode (base is forced to 0, limits are ignored) but must be loaded with
valid selectors pointing to present segments in the GDT, or the CPU faults.

```
CS:  selector = 0x08 (GDT entry 1 × 8 bytes per entry)
     base = 0, limit = 0xFFFF_FFFF, type = 11 (code, exec/read, accessed)
     present = 1, dpl = 0, db = 0, long = 1, granularity = 1

DS/ES/FS/GS/SS:
     selector = 0x10 (GDT entry 2 × 8 bytes per entry)
     base = 0, limit = 0xFFFF_FFFF, type = 3 (data, read/write, accessed)
     present = 1, dpl = 0, db = 1, long = 0, granularity = 1
```

#### General registers

```
RIP    = kernel entry point (from kernel.rs, the ELF e_entry)
RSI    = BOOT_PARAMS_ADDR (0x7000 — pointer to struct boot_params)
RFLAGS = 0x2 (bit 1 is reserved and must always be 1)
RSP    = 0x0 (the kernel sets up its own stack immediately)
```

All other general-purpose registers (RAX, RBX, RCX, RDX, RDI, RBP, R8-R15) are
set to 0.

### The code

```rust
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
    // One entry covering all of guest RAM as usable.
    params.e820_table[0] = linux_loader::bootparam::boot_e820_entry {
        addr: 0,
        size: mem_size,
        type_: E820_RAM,
    };
    params.e820_entries = 1;

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
```

---

## 7. serial.rs — UART emulation

### What a 16550 UART is

The 16550 UART (Universal Asynchronous Receiver/Transmitter) is the standard
serial port on PC-compatible hardware. When the Linux kernel wants to print to a
serial console, it writes bytes to the UART's I/O ports. The UART converts them
to a serial signal on a physical wire.

We don't have a physical wire. Instead, we intercept the kernel's writes to the
UART's I/O ports and forward the bytes to our stdout. From the kernel's
perspective, it's talking to real hardware. From our perspective, it's
`print!()` with extra steps.

### The 16550 register map

The 16550 has 8 registers, accessed via I/O ports 0x3F8 through 0x3FF (for
COM1):

```
Port    Offset  Register (when reading)    Register (when writing)
──────  ──────  ─────────────────────────  ─────────────────────────
0x3F8   0       RBR (Receive Buffer)       THR (Transmit Holding)
0x3F9   1       IER (Interrupt Enable)     IER
0x3FA   2       IIR (Interrupt ID)         FCR (FIFO Control)
0x3FB   3       LCR (Line Control)         LCR
0x3FC   4       MCR (Modem Control)        MCR
0x3FD   5       LSR (Line Status)          —
0x3FE   6       MSR (Modem Status)         —
0x3FF   7       SCR (Scratch)              SCR
```

The only register that matters for our MVP is THR (offset 0): when the kernel
writes a byte here, it's sending a character. We capture that byte and write it
to stdout. The rest of the registers handle interrupts, FIFO control, and modem
signals that we don't need for output-only serial.

The LSR (Line Status Register, offset 5) is also important: the kernel reads
this to check if the transmit buffer is empty before sending another byte. We
always report "empty and ready" so the kernel never waits.

`vm-superio`'s `Serial` type handles all of this state management — we just need
to wire it up and provide an output sink.

> **Spec reference:** The 16550 register set is defined in the
> [National Semiconductor PC16550D datasheet](https://www.ti.com/lit/ds/symlink/pc16550d.pdf).
> The Linux kernel's 8250/16550 driver is in
> [drivers/tty/serial/8250/](https://github.com/torvalds/linux/tree/master/drivers/tty/serial/8250).

### PIO vs MMIO

The 16550 is accessed via **port I/O (PIO)**, not memory-mapped I/O (MMIO). The
difference:

- **PIO** uses dedicated CPU instructions (`in` and `out`) to read/write
  numbered I/O ports (0x0000–0xFFFF). These instructions cause a vmexit that KVM
  delivers to us as `VcpuExit::IoIn` or `VcpuExit::IoOut`.
- **MMIO** uses normal memory load/store instructions to special address ranges
  that aren't backed by RAM. These cause a vmexit delivered as
  `VcpuExit::MmioRead` or `VcpuExit::MmioWrite`.

PIO is a legacy x86 mechanism that doesn't exist on ARM. This is one of the
reasons the naos-macos (aarch64) serial implementation will differ — it'll use
MMIO instead.

### Wiring vm-superio's Serial to an EventFd

`vm-superio`'s `Serial` is generic over three type parameters:
`Serial<T: Trigger, EV: SerialEvents, W: Write>` — an interrupt trigger, an
events hook, and an output sink. Two of them need adapting:

- **The trigger.** `Serial` raises an interrupt by calling `Trigger::trigger`.
  The natural trigger is a `vmm-sys-util` `EventFd`, but `EventFd` does not
  implement `vm_superio::Trigger` — both are foreign types, so the orphan rule
  forbids a direct impl. We define a one-field newtype, `EventFdTrigger`, and
  implement `Trigger` on it (plus `Deref` to the inner `EventFd` for
  convenience). This is the canonical rust-vmm pattern; Firecracker ships the
  same wrapper. We never actually pull the trigger in the MVP, but the type has
  to be there to satisfy the bound.
- **The events hook.** We don't track per-byte serial events, so we use
  vm-superio's built-in no-op type, `NoEvents`. `Serial::new(trigger, out)`
  selects it automatically, so the concrete type is
  `Serial<EventFdTrigger, NoEvents, Stdout>`.

That three-parameter type is what threads through `serial.rs`, `vcpu.rs`, and
`vmm.rs` wherever a `Serial` is named.

### The code

```rust
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
```

---

## 8. vcpu.rs — the run loop

### The vmexit/vmentry cycle

This is the heartbeat of any KVM-based VMM. The cycle is:

```
┌──────────────────────────────────────────────────────────┐
│                                                          │
│  ┌────────────┐   KVM_RUN ioctl    ┌──────────────────┐  │
│  │            │ ─────────────────► │                  │  │
│  │   naos     │                    │   Guest kernel   │  │
│  │ (userspace)│ ◄───────────────── │  (hardware VM)   │  │
│  │            │    vmexit          │                  │  │
│  └─────┬──────┘                    └──────────────────┘  │
│        │                                                 │
│        │  On vmexit, naos inspects the reason:           │
│        │                                                 │
│        ├─ IoOut to serial port → write byte to stdout    │
│        ├─ IoIn from serial port → return register value  │
│        ├─ Hlt → kernel halted, break the loop            │
│        └─ Anything else → bug, log and bail              │
│                                                          │
│  Then call KVM_RUN again to re-enter the guest.          │
│  This loop runs until the kernel halts or we error out.  │
└──────────────────────────────────────────────────────────┘
```

When the vCPU is running guest code, our process is blocked in the `KVM_RUN`
ioctl. The CPU is literally executing guest instructions in hardware (via VT-x's
VMLAUNCH/VMRESUME or AMD-V's VMRUN). We don't get control back until something
happens that the hardware can't handle alone — a "vmexit."

For our MVP, the only expected vmexits are:

1. **`IoOut` to serial ports (0x3F8–0x3FF)**: the kernel executed an `out`
   instruction to write a byte to the UART. We handle it in serial.rs and
   re-enter the guest.
2. **`IoIn` from serial ports (0x3F8–0x3FF)**: the kernel executed an `in`
   instruction to read a UART register (usually LSR to check transmit status).
   We return the register value and re-enter.
3. **`Hlt`**: the kernel executed a `hlt` instruction, which means it has
   nothing more to do. After the kernel panic, it enters an infinite halt loop.
   We break out and exit cleanly.
4. **Anything else**: a vmexit we don't handle. This is a bug in our setup — it
   means the guest is doing something (MMIO access, MSR access, CPUID, etc.)
   that we haven't accounted for. We log it and bail.

> **Spec reference:** The list of KVM exit reasons is in the
> [KVM API docs, section 5](https://docs.kernel.org/virt/kvm/api.html#kvm-run),
> and in `kvm_bindings::KVM_EXIT_*` constants.

### The code

```rust
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
                }
                // Ports outside the serial range are silently ignored.
                // A real VMM would log these as unexpected; for the MVP,
                // the kernel might probe other ports (e.g., 0x80 for
                // POST codes) and we don't want to crash on them.
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
```

> **Version note (kvm-ioctls 0.25):** `VcpuFd::run` takes `&mut self` in current
> kvm-ioctls; older releases took `&self`. That's why `run` here threads a
> `&mut VcpuFd` and `Vmm::run` calls it as `vcpu::run(&mut self.vcpu, …)`.

---

## 9. vmm.rs — tying it together

### Initialization order matters

KVM's API has ordering constraints that aren't obvious from the type system:

1. **Create VM before anything else** — the VM fd is required for creating vCPUs
   and registering memory.
2. **Set TSS address before creating vCPUs** (Intel only, harmless on AMD).
3. **Create IRQ chip before creating vCPUs** — the in-kernel IRQ chip must exist
   before vCPU setup.
4. **Register memory before loading the kernel** — the kernel loader writes
   segments into guest memory.
5. **Configure vCPU registers last** — after memory is set up and the kernel is
   loaded, because we need the entry address from the kernel loader.

### The code

```rust
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
```

---

## 10. main.rs — entry point

```rust
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
```

---

## 11. Running it

### Prerequisites

1. An x86_64 Linux host with `/dev/kvm` and your user in the `kvm` group. See
   `DEVELOPMENT.md`.
2. A test vmlinux built from source with `tinyconfig` + serial support. See
   `DEVELOPMENT.md` "Building the test kernel."
3. Rust stable toolchain. `rust-toolchain.toml` handles this.

### Build and run

```bash
cd ~/code/naos
just build
just run --kernel testdata/vmlinux --mem 256
```

### Expected output

You should see kernel boot messages scrolling on stdout:

```
[    0.000000] Linux version 6.12.0 (gcc ...) #1 ...
[    0.000000] Command line: console=ttyS0 reboot=k panic=1 pci=off
[    0.000000] BIOS-provided physical RAM map:
[    0.000000] BIOS-e820: [mem 0x0000000000000000-0x000000000fffffff] usable
...
[    0.xxxxxx] Run /init as init process
[    0.xxxxxx] Run /sbin/init as init process
[    0.xxxxxx] Kernel panic - not syncing: No working init found.
```

naos-linux exits with status 0. That kernel panic is the success signal.

---

## 12. What happens during boot

A step-by-step trace through the entire boot sequence, from `main()` to kernel
panic:

1. **`main()` parses args**, hands them to `Vmm::new()`.
2. **`Vmm::new()` opens `/dev/kvm`**, creates a VM fd, sets TSS address, creates
   IRQ chip.
3. **`memory::build(256)` allocates 256 MiB** via `mmap(2)`. The host kernel
   gives us a 256 MiB anonymous mapping. `memory::register()` tells KVM "map
   guest physical 0–256 MiB to this host region."
4. **`kernel::load()` reads the vmlinux ELF**, copies segments into guest memory
   at their linker-specified addresses (around 0x1000000). Returns the ELF entry
   point.
5. **`boot::write_cmdline()` writes
   `"console=ttyS0 reboot=k panic=1 pci=off\0"`** to guest address 0x20000.
6. **`boot::write_boot_params()` writes a `struct boot_params`** to guest
   address 0x7000, with one e820 entry (0–256 MiB = RAM) and the cmdline pointer
   (0x20000).
7. **`boot::configure()` writes the GDT** (3 entries at 0x500), **page tables**
   (PML4/PDPT/PD at 0x1000/0x2000/0x3000, identity mapping the first 1 GiB), and
   **sets CPU registers** (CR0 with paging and protection, CR3 pointing to PML4,
   CR4 with PAE, EFER with LME+LMA, CS/DS with GDT selectors, RIP at kernel
   entry, RSI at 0x7000).
8. **`vmm.run()` calls `vcpu::run()`**, which enters the main loop.
9. **First `KVM_RUN`**: the CPU enters guest mode via VMLAUNCH. The first
   instruction the guest executes is at the kernel's `startup_64` entry point.
10. **The kernel's `startup_64`** immediately replaces our GDT, page tables, and
    stack with its own. Our boot setup was a trampoline — valid only long enough
    for the kernel to take over. This happens in the first ~100 instructions.
11. **The kernel initializes its subsystems** — memory management, scheduler,
    timekeeping, console drivers. During this process, it finds `console=ttyS0`
    on the command line and initializes the 8250/16550 serial driver.
12. **Every `printk()` during boot** writes characters to the serial port via
    `out 0x3F8, <byte>`. Each write causes a vmexit. naos catches the `IoOut`,
    calls `serial::handle_write()`, which calls `serial.write(0, byte)`, which
    writes the byte to stdout. This is how boot messages appear on our terminal.
13. **The kernel reads LSR** (port 0x3FD, offset 5) before each write to check
    if the transmit buffer is ready. `serial.read(5)` returns a value with the
    "transmitter empty" bit set, so the kernel never waits.
14. **After initialization, the kernel tries to run `/init`**, then
    `/sbin/init`, then `/etc/init`, then `/bin/init`, then `/bin/sh`. None exist
    (no rootfs). It prints "No working init found."
15. **`panic=1` on the cmdline** means "reboot after 1 second on panic." The
    kernel enters the panic path, prints the panic message, waits 1 second, then
    calls `machine_restart()`, which on our minimal VM executes `hlt`.
16. **The `hlt` instruction causes a vmexit** with reason `VcpuExit::Hlt`.
    `vcpu::run()` breaks out of the loop and returns `Ok(())`. `main()` exits
    with status 0.

---

## 13. What's next

This walkthrough covers the MVP: boots a kernel, sees dmesg, exits clean. The
architecture holds for everything that comes next:

**The immediate next steps** (each one earns a design doc before
implementation):

- **virtio-blk** — add a block device backed by an ext4 image, build the MMIO
  bus and IRQ routing it requires, and boot to a shell. This is the single
  biggest unlock.
- **Serial input** — wire host stdin to the UART RX path, which requires an
  event loop (epoll or similar). Once you have an event loop, every future
  device becomes cheap.
- **Multiple vCPUs** — move the vCPU run loop to a dedicated thread, add vCPU
  coordination. Requires MPTable or ACPI MADT so the kernel discovers the extra
  CPUs.

**The platform trajectory** (see `ARCHITECTURE.md`):

- **naos-macos** — the same walkthrough, different hypervisor API and guest
  architecture. Walkthrough for that will live alongside this one.
- **naos-vmm** — the abstraction layer extracted from naos-linux and naos-macos
  once both exist. No walkthrough until we build it.

---

## Reference links

- **Intel SDM** (Software Developer's Manual):
  [Volume 3: System Programming Guide](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html)
  — covers control registers, GDT, paging, long mode entry.
- **AMD APM** (Architecture Programmer's Manual):
  [Volume 2: System Programming](https://www.amd.com/en/search/documentation/hub.html)
  — equivalent content, sometimes clearer than Intel.
- **KVM API documentation**:
  [Documentation/virt/kvm/api.rst](https://docs.kernel.org/virt/kvm/api.html) —
  every ioctl, every exit reason.
- **Linux boot protocol**:
  [Documentation/arch/x86/boot.rst](https://www.kernel.org/doc/html/latest/arch/x86/boot.html)
  — struct boot_params, e820 map, cmdline.
- **ELF specification**:
  [System V ABI](https://refspecs.linuxfoundation.org/elf/elf.pdf) — program
  headers, segment loading.
- **16550 UART datasheet**:
  [TI PC16550D](https://www.ti.com/lit/ds/symlink/pc16550d.pdf) — register map,
  FIFO behavior.
- **rust-vmm crates on docs.rs**: [kvm-ioctls](https://docs.rs/kvm-ioctls),
  [vm-memory](https://docs.rs/vm-memory),
  [linux-loader](https://docs.rs/linux-loader),
  [vm-superio](https://docs.rs/vm-superio).
