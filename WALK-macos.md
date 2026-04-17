# naos-macos: a first-principles walkthrough

This document walks through every file in `naos-macos` — a minimum viable
Hypervisor.framework-based hypervisor that boots an aarch64 Linux kernel to
dmesg on an Apple Silicon Mac and exits cleanly. It is the companion to
`docs/walkthroughs/naos-linux.md` and follows the same structure: each file in
dependency order, with the actual code, diagrams, spec references, and "why not
the alternative" notes.

**The honesty caveat:** This walkthrough is written before `naos-macos` has been
built. The naos-linux walkthrough was written with the benefit of knowing
exactly what works; this one is written with a mix of specification reading,
reference implementations (libkrun, UTM, QEMU's HVF backend), and architectural
knowledge. Some details — particularly around Hypervisor.framework's exact Rust
binding ergonomics and the device tree's minimal viable shape — will need
revision when the code is actually written. Open questions are flagged
explicitly.

**What we're building:** a ~400-line Rust program that creates a
Hypervisor.framework VM on macOS, allocates guest memory, loads an aarch64 Linux
kernel Image, constructs a minimal Flattened Device Tree describing the virtual
hardware, configures a vCPU at EL1 with MMU off, emulates a UART so the kernel
can print to our terminal, and runs until the kernel panics because there is no
init process.

**What success looks like:**

```
[    0.000000] Booting Linux on physical CPU 0x0000000000 [0x00000000]
[    0.000000] Linux version 6.12.0 ...
[    0.000000] Machine model: naos virtual machine
[    0.000000] Command line: console=ttyAMA0 reboot=k panic=1
...
[    0.123456] Kernel panic - not syncing: No working init found.
```

Same win condition as naos-linux — the kernel panic means everything worked.

---

## Table of contents

1. [Architecture overview](#1-architecture-overview)
2. [ARM64 vs x86_64: what's different](#2-arm64-vs-x86_64-whats-different)
3. [The guest physical memory map](#3-the-guest-physical-memory-map)
4. [Dependencies](#4-dependencies)
5. [memory.rs — guest memory](#5-memoryrs--guest-memory)
6. [kernel.rs — loading the kernel](#6-kernelrs--loading-the-kernel)
7. [dtb.rs — the device tree](#7-dtbrs--the-device-tree)
8. [boot.rs — CPU state setup](#8-bootrs--cpu-state-setup)
9. [serial.rs — UART emulation](#9-serialrs--uart-emulation)
10. [vcpu.rs — the run loop](#10-vcpurs--the-run-loop)
11. [vmm.rs — tying it together](#11-vmmrs--tying-it-together)
12. [main.rs — entry point](#12-mainrs--entry-point)
13. [Running it](#13-running-it)
14. [What happens during boot](#14-what-happens-during-boot)
15. [What's next](#15-whats-next)

---

## 1. Architecture overview

The architecture is structurally identical to naos-linux — a userspace process
that drives a kernel-level hypervisor — but every concrete detail is different.
macOS has a different hypervisor API, the CPU is ARM instead of x86, and the
boot protocol the kernel expects is fundamentally different.

```
┌──────────────────────────────────────────────────────────┐
│  macOS (XNU kernel)                                      │
│  ┌──────────────────────────────────────────────────────┐│
│  │  Hypervisor.framework                                ││
│  │                                                      ││
│  │  Manages:                                            ││
│  │  - VM contexts (guest containers)                    ││
│  │  - vCPU objects (virtual processors)                 ││
│  │  - Memory mappings (guest ↔ host address space)      ││
│  │                                                      ││
│  │  Executes guest code via ARM virtualization           ││
│  │  extensions (EL2 → EL1 transition)                   ││
│  │                                                      ││
│  │  NOTE: No in-kernel device emulation. Unlike KVM,    ││
│  │  HVF does not emulate PIC/IOAPIC/PIT. ALL device     ││
│  │  emulation is our responsibility.                    ││
│  └──────────────────────────────────────────────────────┘│
│                         ▲                                │
│                         │ C function calls               │
│                         │ (not ioctls — this is a        │
│                         │  userspace framework)          │
│                         │                                │
│  ┌──────────────────────┴───────────────────────────────┐│
│  │  naos-macos (our userspace VMM)                      ││
│  │                                                      ││
│  │  Responsibilities:                                   ││
│  │  1. Allocate guest memory (mmap)                     ││
│  │  2. Load the kernel into guest memory                ││
│  │  3. Build a device tree describing virtual hardware  ││
│  │  4. Configure vCPU registers for EL1 entry           ││
│  │  5. Run the vCPU (hv_vcpu_run in a loop)             ││
│  │  6. Handle VM exits (MMIO to UART, PSCI calls)       ││
│  │  7. Emulate ALL devices (no kernel help)             ││
│  └──────────────────────────────────────────────────────┘│
└──────────────────────────────────────────────────────────┘
```

### Key difference from KVM: no in-kernel devices

This is the single biggest architectural difference and it affects everything.

On Linux, KVM emulates the PIC, IOAPIC, and PIT inside the kernel. When the
guest configures interrupts, KVM handles it — naos-linux never sees interrupt
controller traffic. On macOS, Hypervisor.framework provides **raw CPU
virtualization only**. There is no in-kernel interrupt controller, no in-kernel
timer, no in-kernel anything. Every device the guest interacts with must be
emulated by our userspace code.

For the MVP this is actually fine — we don't deliver interrupts anyway (single
vCPU, no IRQ-driven devices, serial is polled). But it means the gap between MVP
and MVP+1 is bigger on macOS than on Linux: adding virtio-blk on naos-macos
requires building interrupt injection ourselves, while naos-linux can lean on
KVM's in-kernel IOAPIC.

### The Hypervisor.framework API

Unlike KVM's ioctl-based API, Hypervisor.framework is a C function library. No
file descriptors, no ioctl numbers — just C function calls that return error
codes:

```
hv_vm_create()                          ← create a VM (once per process)
hv_vm_map(host_addr, guest_addr, size, flags)  ← map memory into guest
hv_vcpu_create(&vcpu, &exit, NULL)      ← create a vCPU
hv_vcpu_set_sys_reg(vcpu, reg, value)   ← set a system register
hv_vcpu_set_reg(vcpu, reg, value)       ← set a general register
hv_vcpu_run(vcpu)                       ← enter guest mode
hv_vcpu_exit_info(vcpu)                 ← inspect why the guest exited
hv_vcpu_destroy(vcpu)                   ← tear down vCPU
hv_vm_destroy()                         ← tear down VM
```

The API is simpler than KVM's in some ways (no ioctl ceremony, no capability
negotiation) and more manual in others (you set registers one at a time instead
of in bulk structs).

> **Spec reference:** The Hypervisor.framework API is documented at
> [developer.apple.com/documentation/hypervisor](https://developer.apple.com/documentation/hypervisor).
> The ARM64-specific parts are under `hv_vcpu_set_sys_reg` and related
> functions.

---

## 2. ARM64 vs x86_64: what's different

Before diving into code, it's worth understanding the fundamental architectural
differences that shape every file in naos-macos.

### No segmentation

x86_64 has the GDT, segment registers, segment selectors — an entire
segmentation layer that exists for historical reasons and must be configured
even though long mode largely ignores it. ARM64 has no segmentation at all.
There is no GDT. There are no segment registers. This eliminates the most
painful part of naos-linux's boot.rs entirely.

### Exception levels instead of rings

x86_64 has ring 0 (kernel) and ring 3 (user), selected via segment descriptors.
ARM64 has four exception levels:

```
EL3 — Secure Monitor (firmware, TrustZone)
EL2 — Hypervisor (this is where Hypervisor.framework runs)
EL1 — OS kernel (this is where the Linux guest runs)
EL0 — User applications
```

Hypervisor.framework creates vCPUs at EL1. We configure the EL1 system registers
and the kernel runs there. We never touch EL2 or EL3 — Hypervisor.framework
manages EL2 and there's no EL3 in a VM.

### Different page table format

ARM64 uses a completely different page table format from x86_64. But here's the
key insight for the MVP: **we don't build page tables at all.** The ARM64 Linux
boot protocol specifies that the kernel should be entered with the MMU _off_.
The kernel builds its own page tables during startup. This eliminates the entire
page table section of boot.rs.

Compare:

- **naos-linux boot.rs**: must build GDT + page tables + set CR0/CR3/CR4/EFER =
  ~100 complex lines
- **naos-macos boot.rs**: set a handful of system registers + general registers
  = ~30 lines

### Device tree instead of BIOS/ACPI

x86_64 machines use BIOS tables and ACPI to describe hardware. The kernel
discovers memory via e820, devices via ACPI, interrupts via the IOAPIC. We faked
this with a `struct boot_params` containing an e820 map.

ARM64 machines use a **Flattened Device Tree (FDT/DTB)** — a binary data
structure that describes every piece of hardware: CPUs, memory, UARTs, interrupt
controllers, timers, everything. The bootloader passes the DTB address in
register `x0`, and the kernel parses it to discover the machine.

This is the single biggest new piece in naos-macos: building a correct DTB.
There's no boot_params struct to fill in — we construct a tree of nodes
describing our virtual machine's hardware. The DTB replaces boot_params, e820,
and ACPI tables all at once.

### MMIO instead of PIO

x86_64 has a dedicated I/O port address space accessed via `in`/`out`
instructions. ARM64 has no port I/O. All device registers are memory-mapped —
you access a UART by reading/writing specific physical addresses, not I/O port
numbers. This means:

- Guest writes to the UART register address cause an MMIO vmexit (not a PIO
  vmexit)
- We need to reserve a region of guest physical address space for the UART (not
  just port numbers)
- The DTB must describe where the UART's registers live in the physical address
  space

### Different UART: PL011 instead of 16550

The 16550 UART is an x86 convention. ARM systems typically use the **ARM
PrimeCell PL011** UART, which has a different register layout, different
register offsets, and different behavior. The Linux kernel has a dedicated
driver for it (`drivers/tty/amba-pl011.c`), and `console=ttyAMA0` selects it.

However, ARM64 Linux also supports the 16550 over MMIO (`console=ttyS0`), and
`vm-superio`'s Serial type is pure logic that works on any platform. The choice
between PL011 and 16550-over-MMIO is a real tradeoff we'll evaluate in the
serial.rs section.

### Summary table

```
                    naos-linux (x86_64)         naos-macos (aarch64)
─────────────────   ───────────────────────     ───────────────────────
Hypervisor API      KVM (/dev/kvm, ioctls)      Hypervisor.framework (C calls)
Segmentation        GDT required (3 entries)    No segmentation
Page tables         Must build (PML4/PDPT/PD)   MMU off at entry, kernel builds own
Boot state          CR0/CR3/CR4/EFER/segments   A few system regs, MMU off
HW discovery        e820 + boot_params          Flattened Device Tree (DTB)
Device I/O          PIO (in/out instructions)   MMIO (load/store to addresses)
Serial device       16550 at ports 0x3F8-0x3FF  PL011 at MMIO address
Kernel format       vmlinux ELF                 Image (flat binary with header)
Entry register      RSI = boot_params address   x0 = DTB address
boot.rs complexity  ~150 lines                  ~30 lines
New complexity      (none — simpler than x86)   dtb.rs (~100-150 lines)
```

The total complexity is roughly equivalent — x86_64 front-loads it in boot.rs,
ARM64 spreads it into dtb.rs.

---

## 3. The guest physical memory map

ARM64 is more flexible than x86_64 about memory layout — there's no legacy BIOS
area, no ISA hole, no mandatory addresses. But the Linux kernel still has
conventions, and the DTB must accurately describe where everything lives.

```
Guest Physical Address Space (256 MiB)
═══════════════════════════════════════════════════════

0x0000_0000  ┌──────────────────────────────────────┐
             │  (unused, or reserved for ROM/flash)  │
0x0800_0000  ├──────────────────────────────────────┤  ← 128 MiB
             │  Device region (MMIO)                 │
             │  ┌────────────────────────────────┐  │
             │  │  PL011 UART                    │  │
             │  │  0x0900_0000 – 0x0900_0FFF     │  │
             │  │  (4 KiB register space)        │  │
             │  └────────────────────────────────┘  │
             │  ┌────────────────────────────────┐  │
             │  │  GIC Distributor               │  │
             │  │  0x0800_0000 – 0x0800_FFFF     │  │
             │  │  (64 KiB)                      │  │
             │  └────────────────────────────────┘  │
             │  ┌────────────────────────────────┐  │
             │  │  GIC CPU Interface             │  │
             │  │  0x0801_0000 – 0x0801_FFFF     │  │
             │  │  (64 KiB)                      │  │
             │  └────────────────────────────────┘  │
0x4000_0000  ├──────────────────────────────────────┤  ← 1 GiB
             │  RAM                                 │
             │  (256 MiB: 0x4000_0000 – 0x4FFF_FFFF)│
             │                                      │
             │  ┌────────────────────────────────┐  │
             │  │  DTB (Flattened Device Tree)   │  │
             │  │  0x4000_0000 – 0x4000_FFFF     │  │
             │  │  (up to 64 KiB, at RAM base)   │  │
             │  └────────────────────────────────┘  │
             │  ┌────────────────────────────────┐  │
             │  │  Kernel Image                  │  │
             │  │  0x4008_0000                    │  │
             │  │  (TEXT_OFFSET = 0x80000 above   │  │
             │  │   RAM base)                     │  │
             │  └────────────────────────────────┘  │
             │                                      │
0x5000_0000  └──────────────────────────────────────┘  ← end of RAM
```

**Why these addresses?**

- **RAM at 0x40000000 (1 GiB):** This is the conventional DRAM base for ARM
  virtual machines. QEMU's `virt` machine type uses this, and it's what the
  Linux kernel's default configuration expects. Addresses below this are
  reserved for MMIO devices.
- **Kernel at RAM base + 0x80000:** The ARM64 boot protocol
  (`Documentation/arm64/booting.rst`) specifies that the kernel Image must be
  loaded at a 2 MiB-aligned offset from the start of DRAM. The traditional
  offset is 0x80000 (512 KiB), though modern kernels can be loaded at any 2 MiB
  boundary. We use the traditional offset for compatibility.
- **DTB at RAM base:** The DTB is placed at the very beginning of RAM. The
  kernel expects it at the address passed in register `x0`. We put it at the RAM
  base before the kernel to avoid any overlap.
- **UART at 0x09000000:** This is the QEMU `virt` machine convention. The Linux
  kernel has a built-in device tree for QEMU's virt machine that uses this
  address, and many ARM64 kernel configs expect it here. Using the same address
  means less friction with default kernel configs.
- **GIC at 0x08000000:** Same convention as QEMU `virt`. The Generic Interrupt
  Controller's distributor and CPU interface live at these well-known addresses.

We encode these as constants:

```rust
// addr.rs or at the top of the relevant modules

/// Base of guest DRAM. RAM starts at 1 GiB, following the QEMU virt convention.
const RAM_BASE: u64 = 0x4000_0000;

/// Where the DTB is placed in guest memory (at the start of RAM).
const DTB_ADDR: u64 = RAM_BASE;

/// Maximum size of the DTB blob. 64 KiB is generous for our minimal tree.
const DTB_MAX_SIZE: u64 = 0x1_0000;

/// Where the kernel Image is loaded. TEXT_OFFSET above RAM base.
/// ARM64 boot protocol: Documentation/arm64/booting.rst
const KERNEL_LOAD_ADDR: u64 = RAM_BASE + 0x8_0000;

/// PL011 UART base address. QEMU virt convention.
const PL011_BASE: u64 = 0x0900_0000;

/// PL011 UART register space size. The PL011 has registers from offset
/// 0x000 to 0xFFC, but we only need to emulate the first few.
const PL011_SIZE: u64 = 0x1000;

/// GIC v2 distributor base address. QEMU virt convention.
const GICD_BASE: u64 = 0x0800_0000;
const GICD_SIZE: u64 = 0x1_0000;

/// GIC v2 CPU interface base address.
const GICC_BASE: u64 = 0x0801_0000;
const GICC_SIZE: u64 = 0x1_0000;

/// The interrupt ID (SPI) assigned to the UART.
/// QEMU virt uses SPI 1 (which is GIC interrupt 33 = 32 + 1).
const UART_IRQ: u32 = 1;
```

> **Spec reference:** The QEMU `virt` machine layout is defined in
> [hw/arm/virt.c](https://github.com/qemu/qemu/blob/master/hw/arm/virt.c) in the
> QEMU source tree. The address map there is the de facto standard for ARM64
> virtual machines.

---

## 4. Dependencies

```toml
[package]
name = "naos-macos"
version = "0.1.0"
edition = "2021"
publish = false

[dependencies]
# Hypervisor.framework Rust bindings. This crate wraps Apple's C API in
# safe Rust types. The exact crate may change — applevisor, ahv, or
# hand-rolled bindings are all candidates. We pick whichever is actively
# maintained and has the right API shape when stage 2 starts.
#
# OPEN QUESTION: which crate. The walkthrough uses `applevisor` as a
# placeholder. Evaluate before coding.
applevisor = "0.2"

# Device tree construction. Builds a Flattened Device Tree (FDT/DTB) blob
# programmatically. The alternative is hand-writing a .dts file and
# compiling it with dtc at build time, but runtime construction is more
# flexible and avoids a build-time dependency on dtc.
vm-fdt = "0.3"

# Guest memory abstraction. vm-memory's core types (GuestAddress,
# GuestMemoryMmap) are pure Rust with no Linux-specific code. They should
# work on macOS. If they don't, we write a minimal replacement (~50 lines).
#
# OPEN QUESTION: verify vm-memory compiles on macOS. The mmap backend uses
# libc::mmap which exists on macOS, but the feature flags may need adjustment.
vm-memory = { version = "0.16", features = ["backend-mmap"] }

# 16550 UART emulation. We're using the 16550 over MMIO rather than
# emulating a PL011 from scratch — vm-superio gives us the register
# state machine for free. See serial.rs for the rationale.
vm-superio = "0.8"

# Error handling and CLI — same as naos-linux.
anyhow = "1"
clap = { version = "4", features = ["derive"] }

[lints]
workspace = true
```

### Dependency differences from naos-linux

| naos-linux                   | naos-macos                | Why                                                                                               |
| ---------------------------- | ------------------------- | ------------------------------------------------------------------------------------------------- |
| `kvm-ioctls`, `kvm-bindings` | `applevisor` (or similar) | Different hypervisor API                                                                          |
| `linux-loader`               | (manual Image loading)    | ARM64 Image format is trivial to parse — 4 fields in a 64-byte header. A crate would be overhead. |
| `vmm-sys-util`               | (not needed)              | EventFd is Linux-specific. macOS equivalent TBD if we need interrupt triggers.                    |
| —                            | `vm-fdt`                  | DTB construction. naos-linux doesn't need this because x86 uses boot_params/e820.                 |

---

## 5. memory.rs — guest memory

### How HVF memory mapping works

Hypervisor.framework uses `hv_vm_map` instead of KVM's
`KVM_SET_USER_MEMORY_REGION`. The concepts are identical — "this host memory
backs this guest physical address range" — but the API shape differs:

```
KVM (naos-linux):
  ioctl(vm_fd, KVM_SET_USER_MEMORY_REGION, {
      slot: 0,
      guest_phys_addr: 0,
      memory_size: 256 MiB,
      userspace_addr: host_ptr,
      flags: 0,
  })

HVF (naos-macos):
  hv_vm_map(host_ptr, guest_phys_addr, size, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC)
```

Notable differences:

- **No slot index.** HVF identifies regions by their guest address, not by a
  slot number. You can map and unmap arbitrary ranges without managing slots.
- **Explicit permission flags.** HVF requires you to specify read/write/execute
  permissions per mapping. KVM infers them from page table entries (EPT/NPT).
- **No VM fd.** HVF has a global VM context per process (created by
  `hv_vm_create`). Memory mappings are against this global context.

```
Host process address space              Guest physical address space
┌─────────────────────────┐             ┌─────────────────────────┐
│                         │             │  MMIO devices           │
│                         │             │  (not backed by RAM)    │
│                         │             │  0x0800_0000-0x3FFF_FFFF│
│                         │             ├─────────────────────────┤
│  mmap'd region ─────────┼─────────────┼─► 0x4000_0000          │
│  (256 MiB of anon mem)  │  HVF maps   │   Guest RAM             │
│  host_addr: 0x1...      │  these via  │   (256 MiB)             │
│  length: 0x1000_0000    │  Stage 2    │                         │
│                         │  page tables│                         │
│                         │             ├─► 0x5000_0000           │
└─────────────────────────┘             │   End of guest RAM      │
                                        └─────────────────────────┘
```

Note that RAM starts at 0x40000000, not 0x0. The region below RAM is the MMIO
space for devices (UART, GIC). We don't map host memory there — MMIO accesses to
those addresses cause vmexits that we handle in the run loop.

### The code

```rust
// memory.rs
//
// Guest memory allocation and Hypervisor.framework registration.
//
// Conceptually identical to naos-linux's memory.rs: allocate host memory
// via mmap, then register it with the hypervisor as guest physical RAM.
// The API calls differ but the pattern is the same.

use anyhow::{Context, Result};
use applevisor as hv;
use vm_memory::{GuestAddress, GuestMemoryMmap, GuestMemoryRegion};

use crate::addr::RAM_BASE;

/// Build the guest's physical memory.
///
/// Creates a single anonymous mmap region of `size_mib` MiB. Unlike
/// naos-linux, the guest physical address does NOT start at 0 — RAM
/// begins at RAM_BASE (0x40000000). The region below that is MMIO
/// space for devices.
pub fn build(size_mib: u64) -> Result<GuestMemoryMmap> {
    let size_bytes = size_mib
        .checked_mul(1024 * 1024)
        .context("Memory size overflow")?;

    // GuestMemoryMmap maps the region starting at RAM_BASE in guest
    // physical address space. The host side is an anonymous mmap as usual.
    GuestMemoryMmap::from_ranges(&[(GuestAddress(RAM_BASE), size_bytes as usize)])
        .context("Failed to create guest memory via mmap")
}

/// Register guest memory with Hypervisor.framework.
///
/// hv_vm_map takes:
///   - host pointer (where the mmap lives in our address space)
///   - guest physical address (where it appears in the guest)
///   - size
///   - permission flags (read, write, execute)
///
/// The guest needs execute permission because it will run kernel code
/// directly from this memory.
pub fn register(guest_mem: &GuestMemoryMmap) -> Result<()> {
    let region = guest_mem
        .iter()
        .next()
        .context("Guest memory has no regions")?;

    hv::vm_map(
        region.as_ptr() as *mut std::ffi::c_void,
        region.start_addr().raw_value(),
        region.len() as usize,
        hv::MemPerms::READ | hv::MemPerms::WRITE | hv::MemPerms::EXEC,
    )
    .context("hv_vm_map failed")?;

    Ok(())
}
```

> **Open question:** The exact `applevisor` API for `vm_map` may differ from
> what's shown here. The concept is stable (host ptr, guest addr, size, perms),
> but the Rust function signature and error type depend on the binding crate
> chosen. Verify at coding time.

---

## 6. kernel.rs — loading the kernel

### What an ARM64 Image is

ARM64 Linux kernels are distributed as a flat binary called `Image` (capital I).
Unlike x86's vmlinux ELF with program headers and sections, `Image` is a raw
binary with a 64-byte header at the top. The rest is the kernel's code and data,
ready to execute from the first byte after the header.

```
ARM64 Image file
┌──────────────────────────────────────┐
│  Header (64 bytes)                    │
│  ┌────────────────────────────────┐  │
│  │  Offset 0x00: code0 (u32)     │  │  ← branch instruction (skip header)
│  │  Offset 0x04: code1 (u32)     │  │  ← reserved
│  │  Offset 0x08: text_offset(u64)│  │  ← offset from start of RAM to load at
│  │  Offset 0x10: image_size (u64)│  │  ← total size of Image in memory
│  │  Offset 0x18: flags (u64)     │  │  ← endianness, page size, placement
│  │  Offset 0x20: res2 (u64)      │  │  ← reserved
│  │  Offset 0x28: res3 (u64)      │  │  ← reserved
│  │  Offset 0x30: res4 (u64)      │  │  ← reserved
│  │  Offset 0x38: magic (u32)     │  │  ← 0x644D5241 ("ARM\x64")
│  │  Offset 0x3C: res5 (u32)      │  │  ← reserved / PE offset
│  └────────────────────────────────┘  │
│  Kernel code and data                 │
│  (image_size - 64 bytes)              │
│  ...                                  │
└──────────────────────────────────────┘
```

The header tells us:

- **`text_offset`**: where the kernel expects to be loaded, relative to the
  start of DRAM. Traditionally 0x80000 (512 KiB), though modern kernels (v5.8+)
  with bit 3 of `flags` set can be loaded at any 2 MiB-aligned offset.
- **`image_size`**: total memory the kernel needs (code + data + BSS). We must
  ensure this fits in guest RAM.
- **`magic`**: 0x644D5241, which is "ARM\x64" in little-endian. Used to validate
  the file.

The load procedure is: read the header, validate the magic, copy the entire file
to `RAM_BASE + text_offset`, and set PC to the load address. The first
instruction in the Image (the `code0` field) is a branch that jumps over the
header into the actual startup code.

> **Spec reference:** The ARM64 Image header is defined in
> [Documentation/arm64/booting.rst](https://www.kernel.org/doc/html/latest/arch/arm64/booting.html)
> in the Linux kernel tree.

### The code

```rust
// kernel.rs
//
// ARM64 kernel Image loader.
//
// Unlike naos-linux which uses linux-loader for ELF parsing, we parse the
// ARM64 Image header ourselves. The header is 64 bytes with 4 fields we
// care about — a crate would be overkill.

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use crate::addr::{KERNEL_LOAD_ADDR, RAM_BASE};

/// ARM64 Image header magic: "ARM\x64" in little-endian.
const ARM64_IMAGE_MAGIC: u32 = 0x644D_5241;

/// ARM64 Image header (64 bytes at the start of the Image file).
///
/// We only parse the fields we need; the rest are reserved or unused
/// for our purposes.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Arm64ImageHeader {
    /// Branch instruction to skip the header. Executed if the Image
    /// is loaded as a raw binary — the CPU branches past the header
    /// into the real startup code.
    code0: u32,
    code1: u32,

    /// Offset from the start of DRAM to the kernel's text segment.
    /// Traditionally 0x80000 (512 KiB). For kernels with flags bit 3
    /// set, this can be 0 and the loader chooses the offset.
    text_offset: u64,

    /// Total size of the decompressed kernel Image in memory.
    /// Includes .text, .rodata, .data, and .bss.
    image_size: u64,

    /// Flags:
    ///   Bit 0: kernel endianness (0 = LE, 1 = BE)
    ///   Bit 1-2: page size (0 = unspecified)
    ///   Bit 3: image placement (0 = must respect text_offset,
    ///          1 = may be loaded at any 2 MiB boundary)
    flags: u64,

    /// Reserved fields (we read past them to reach the magic).
    _reserved2: u64,
    _reserved3: u64,
    _reserved4: u64,

    /// Magic number: must be ARM64_IMAGE_MAGIC.
    magic: u32,

    /// Reserved / PE header offset.
    _reserved5: u32,
}

/// Load an ARM64 kernel Image into guest memory.
///
/// Reads the 64-byte header to validate the magic and determine the
/// load offset, then copies the entire Image into guest memory at
/// RAM_BASE + text_offset.
///
/// Returns the guest physical address of the kernel entry point
/// (the first byte of the Image in guest memory).
pub fn load(guest_mem: &GuestMemoryMmap, kernel_path: &Path) -> Result<u64> {
    let image_data =
        fs::read(kernel_path).context("Failed to read kernel Image file")?;

    if image_data.len() < 64 {
        bail!("Kernel Image too small ({} bytes, need at least 64 for header)", image_data.len());
    }

    // Parse the header. The Image is little-endian (ARM64 Linux is LE by default).
    let header: Arm64ImageHeader = unsafe {
        std::ptr::read_unaligned(image_data.as_ptr() as *const Arm64ImageHeader)
    };

    if header.magic != ARM64_IMAGE_MAGIC {
        bail!(
            "Invalid ARM64 Image magic: expected 0x{:08X}, got 0x{:08X}",
            ARM64_IMAGE_MAGIC, header.magic
        );
    }

    // Determine where to load the kernel in guest physical address space.
    // If text_offset is 0 and bit 3 of flags is set, the kernel can be
    // loaded at any 2 MiB aligned offset. We use our constant KERNEL_LOAD_ADDR
    // which is RAM_BASE + 0x80000 (the traditional offset).
    let load_addr = if header.text_offset != 0 {
        RAM_BASE + header.text_offset
    } else {
        KERNEL_LOAD_ADDR
    };

    // Write the entire Image into guest memory at the load address.
    // The first instruction (code0) is a branch that skips the header,
    // so the entry point is the load address itself.
    guest_mem
        .write_slice(&image_data, GuestAddress(load_addr))
        .context("Failed to write kernel Image to guest memory")?;

    Ok(load_addr)
}
```

### Why no linux-loader?

The ARM64 Image format is deliberately simple. The header is 64 bytes with 4
meaningful fields. The load procedure is "copy the file to an address."
linux-loader can do this, and if it works on macOS, using it is fine. But the
format is simple enough that hand-parsing avoids a dependency and gives us more
control. If linux-loader's macOS support proves reliable, we can switch to it
later — the interface (takes memory + path, returns entry address) is identical.

---

## 7. dtb.rs — the device tree

This is the file that has no equivalent in naos-linux and represents the biggest
chunk of new work. The DTB is how the kernel discovers everything about the
machine — CPUs, memory, devices, interrupts, timers. Without a correct DTB, the
kernel has no idea what hardware exists and cannot boot.

### What a device tree looks like

A device tree is a hierarchical structure of nodes and properties. Here's what
our minimal tree describes, in human-readable DTS (Device Tree Source) format:

```dts
/ {
    compatible = "naos,virt";
    #address-cells = <2>;
    #size-cells = <2>;

    chosen {
        bootargs = "console=ttyAMA0 reboot=k panic=1";
        stdout-path = "/pl011@9000000";
    };

    memory@40000000 {
        device_type = "memory";
        reg = <0x00 0x40000000 0x00 0x10000000>;  // 256 MiB at 0x40000000
    };

    cpus {
        #address-cells = <1>;
        #size-cells = <0>;

        cpu@0 {
            device_type = "cpu";
            compatible = "arm,arm-v8";
            reg = <0>;
            enable-method = "psci";
        };
    };

    psci {
        compatible = "arm,psci-1.0";
        method = "hvc";
    };

    timer {
        compatible = "arm,armv8-timer";
        interrupts = <1 13 4>,  // secure phys timer, PPI 13
                     <1 14 4>,  // non-secure phys timer, PPI 14
                     <1 11 4>,  // virtual timer, PPI 11
                     <1 10 4>;  // hypervisor timer, PPI 10
        always-on;
    };

    intc: interrupt-controller@8000000 {
        compatible = "arm,gic-400";
        #interrupt-cells = <3>;
        interrupt-controller;
        reg = <0x00 0x08000000 0x00 0x10000>,   // GICD
              <0x00 0x08010000 0x00 0x10000>;    // GICC
    };

    pl011@9000000 {
        compatible = "arm,pl011", "arm,primecell";
        reg = <0x00 0x09000000 0x00 0x1000>;
        interrupts = <0 1 4>;  // SPI 1, level-sensitive active-high
        clocks = <&apb_pclk>;
        clock-names = "uartclk", "apb_pclk";
    };

    apb_pclk: clock {
        compatible = "fixed-clock";
        #clock-cells = <0>;
        clock-frequency = <24000000>;  // 24 MHz (conventional for virt)
        clock-output-names = "clk24mhz";
    };
};
```

That looks like a lot, but each node is there for a specific reason:

- **`/chosen`**: the kernel command line and stdout path. Equivalent to
  naos-linux's `cmd_line_ptr` in boot_params.
- **`/memory`**: tells the kernel where RAM is and how big. Equivalent to the
  e820 map.
- **`/cpus/cpu@0`**: one CPU, ARMv8, PSCI-managed. Required or the kernel
  doesn't know it has a processor.
- **`/psci`**: Power State Coordination Interface. Tells the kernel how to do
  power management (halt, reboot, bring up secondary CPUs). "method = hvc" means
  PSCI calls use the HVC instruction, which traps to EL2 (our hypervisor). This
  is how the kernel halts — it makes a PSCI SYSTEM_OFF call via HVC, which we
  catch as a vmexit.
- **`/timer`**: the ARM generic timer. The kernel uses this for timekeeping. The
  timer is built into the CPU; we just need to tell the kernel which interrupts
  it uses.
- **`/intc`**: the GIC (Generic Interrupt Controller). Even though the MVP
  doesn't deliver interrupts, the kernel won't boot without an interrupt
  controller node — it crashes in `irqchip_init()` during early boot.
- **`/pl011`**: the UART. Tells the kernel there's a PL011 at address
  0x09000000.
- **`/clock`**: a fixed clock that the PL011 driver requires. Without this, the
  PL011 driver refuses to initialize because it can't determine the baud rate.

> **Spec reference:** The Device Tree specification is at
> [devicetree.org/specifications](https://www.devicetree.org/specifications/).
> The ARM-specific bindings are in the Linux kernel's
> `Documentation/devicetree/bindings/arm/` directory.

### The code

```rust
// dtb.rs
//
// Flattened Device Tree (FDT/DTB) construction for the naos virtual machine.
//
// This module builds a minimal device tree describing the virtual hardware:
// one CPU, one memory region, one UART, an interrupt controller, a timer,
// and a PSCI node. The kernel uses this to discover the machine's hardware
// during boot.
//
// The DTB is built at runtime using the vm-fdt crate rather than compiled
// from a .dts file. This avoids a build-time dependency on dtc (the device
// tree compiler) and makes the tree programmatic — we can adjust memory
// size and device addresses based on runtime parameters.
//
// The structure mirrors QEMU's virt machine, which is the de facto standard
// for ARM64 virtual machines. Using the same addresses and compatible
// strings means the kernel's built-in drivers work without custom configs.

use anyhow::{Context, Result};
use vm_fdt::FdtWriter;

use crate::addr::*;

/// Build a minimal device tree blob for the naos virtual machine.
///
/// Returns the raw DTB bytes, ready to be written into guest memory.
///
/// Arguments:
///   - `mem_size`: guest RAM size in bytes
///   - `cmdline`: kernel command line string
pub fn build(mem_size: u64, cmdline: &str) -> Result<Vec<u8>> {
    let mut fdt = FdtWriter::new()
        .context("Failed to create FDT writer")?;

    // --- Root node ---
    // #address-cells = 2: addresses are 64-bit (two 32-bit cells)
    // #size-cells = 2: sizes are 64-bit (two 32-bit cells)
    let root = fdt.begin_node("")?;
    fdt.property_string("compatible", "naos,virt")?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;

    // --- /chosen ---
    // Kernel command line and stdout device path.
    {
        let chosen = fdt.begin_node("chosen")?;
        fdt.property_string("bootargs", cmdline)?;
        fdt.property_string("stdout-path", "/pl011@9000000")?;
        fdt.end_node(chosen)?;
    }

    // --- /memory ---
    // One region of usable RAM. Equivalent to naos-linux's e820 entry.
    {
        let memory = fdt.begin_node(&format!("memory@{:x}", RAM_BASE))?;
        fdt.property_string("device_type", "memory")?;
        // reg = <addr_hi addr_lo size_hi size_lo>
        fdt.property_array_u64("reg", &[RAM_BASE, mem_size])?;
        fdt.end_node(memory)?;
    }

    // --- /cpus ---
    {
        let cpus = fdt.begin_node("cpus")?;
        fdt.property_u32("#address-cells", 1)?;
        fdt.property_u32("#size-cells", 0)?;

        // /cpus/cpu@0 — one ARMv8 CPU
        {
            let cpu = fdt.begin_node("cpu@0")?;
            fdt.property_string("device_type", "cpu")?;
            fdt.property_string("compatible", "arm,arm-v8")?;
            fdt.property_u32("reg", 0)?;
            // PSCI is the standard ARM power management interface.
            // "enable-method" tells the kernel how to bring up secondary
            // CPUs (irrelevant for MVP single-vCPU, but the property is
            // expected).
            fdt.property_string("enable-method", "psci")?;
            fdt.end_node(cpu)?;
        }

        fdt.end_node(cpus)?;
    }

    // --- /psci ---
    // Power State Coordination Interface. The kernel uses this to halt,
    // reboot, and manage CPU power states. "method = hvc" means PSCI
    // calls use the HVC instruction, which traps to EL2. We catch the
    // resulting vmexit and handle SYSTEM_OFF / SYSTEM_RESET.
    {
        let psci = fdt.begin_node("psci")?;
        fdt.property_string("compatible", "arm,psci-1.0")?;
        fdt.property_string("method", "hvc")?;
        fdt.end_node(psci)?;
    }

    // --- /timer ---
    // ARM Generic Timer. Built into the CPU; we just describe which
    // interrupts it uses. The kernel needs this for timekeeping.
    //
    // Each interrupt is <type irq_num flags>:
    //   type 1 = PPI (per-processor interrupt)
    //   flags 4 = level-sensitive, active-high (IRQ_TYPE_LEVEL_HIGH)
    {
        let timer = fdt.begin_node("timer")?;
        fdt.property_string("compatible", "arm,armv8-timer")?;
        // Four timer interrupts: secure phys, non-secure phys, virtual, hyp
        fdt.property_array_u32("interrupts", &[
            1, 13, 4, // secure physical timer, PPI 13
            1, 14, 4, // non-secure physical timer, PPI 14
            1, 11, 4, // virtual timer, PPI 11
            1, 10, 4, // hypervisor timer, PPI 10
        ])?;
        fdt.property_null("always-on")?;
        fdt.end_node(timer)?;
    }

    // --- /intc (GIC v2) ---
    // Generic Interrupt Controller. Even though the MVP doesn't deliver
    // interrupts, the kernel panics during irqchip_init() without this
    // node. The GIC addresses match the QEMU virt convention.
    //
    // The phandle (interrupt_phandle) is used by other nodes to reference
    // this interrupt controller.
    let interrupt_phandle = 1u32;
    {
        let intc = fdt.begin_node(&format!("intc@{:x}", GICD_BASE))?;
        fdt.property_string("compatible", "arm,gic-400")?;
        fdt.property_u32("#interrupt-cells", 3)?;
        fdt.property_null("interrupt-controller")?;
        fdt.property_u32("phandle", interrupt_phandle)?;
        // reg: GICD base+size, GICC base+size
        fdt.property_array_u64("reg", &[
            GICD_BASE, GICD_SIZE,
            GICC_BASE, GICC_SIZE,
        ])?;
        fdt.end_node(intc)?;
    }

    // --- /pl011 (UART) ---
    // The serial port the kernel writes boot messages to.
    // compatible = "arm,pl011" matches the kernel's AMBA PL011 driver.
    // The "arm,primecell" compatible is required for the AMBA bus probe.
    //
    // The interrupt line is SPI 1 (type=0 means SPI, irq=1, flags=4).
    // SPI 1 = GIC interrupt number 33 (SPIs start at 32).
    let clock_phandle = 2u32;
    {
        let uart = fdt.begin_node(&format!("pl011@{:x}", PL011_BASE))?;
        fdt.property_string_list("compatible", &["arm,pl011", "arm,primecell"])?;
        fdt.property_array_u64("reg", &[PL011_BASE, PL011_SIZE])?;
        // interrupt: SPI 1, level-high
        fdt.property_array_u32("interrupts", &[0, UART_IRQ, 4])?;
        fdt.property_u32("interrupt-parent", interrupt_phandle)?;
        fdt.property_array_u32("clocks", &[clock_phandle, clock_phandle])?;
        fdt.property_string_list("clock-names", &["uartclk", "apb_pclk"])?;
        fdt.end_node(uart)?;
    }

    // --- /clock ---
    // Fixed 24 MHz clock for the PL011 UART. The PL011 driver requires
    // a clock source to determine the baud rate. Without this node, the
    // driver refuses to probe and the kernel prints nothing.
    {
        let clock = fdt.begin_node("apb-pclk")?;
        fdt.property_string("compatible", "fixed-clock")?;
        fdt.property_u32("#clock-cells", 0)?;
        fdt.property_u32("clock-frequency", 24_000_000)?;
        fdt.property_string("clock-output-names", "clk24mhz")?;
        fdt.property_u32("phandle", clock_phandle)?;
        fdt.end_node(clock)?;
    }

    fdt.end_node(root)?;

    fdt.finish()
        .context("Failed to finalize FDT blob")
}
```

> **Open question:** The exact `vm-fdt` API shown above is based on the crate's
> documented interface. Some method names may differ between versions (e.g.,
> `property_string_list` vs `property_array_string`). Verify at coding time. The
> structure — nodes, properties, phandle references — is standard FDT regardless
> of which library builds it.

> **Open question:** Whether a GIC node is truly required for the MVP, or
> whether the kernel can boot with `noapic`-equivalent options on aarch64.
> Testing will tell. If we can drop the GIC node, the DTB becomes significantly
> simpler. But it's safer to include it.

---

## 8. boot.rs — CPU state setup

This is where naos-macos pays off the architectural bet. ARM64 boot setup is
dramatically simpler than x86_64, because there's no segmentation, no page
tables to build (MMU off at entry), and no long-mode transition dance.

### What the kernel expects at entry

From `Documentation/arm64/booting.rst`:

```
  Register  Value
  ────────  ──────────────────────────────────────────
  x0        Physical address of the DTB in memory
  x1        0 (reserved for future use)
  x2        0 (reserved for future use)
  x3        0 (reserved for future use)
  PC        Kernel entry point (load address of Image)

  CPU state:
  - Exception level: EL1 (OS kernel level)
  - MMU: OFF
  - D-cache: ON (may be on or off; kernel handles both)
  - I-cache: ON (may be on or off)
  - Interrupts: masked (PSTATE.DAIF = all 1s)
  - Endianness: little-endian (SCTLR_EL1.EE = 0)
  - FP/SIMD: accessible (CPACR_EL1.FPEN = 0b11)
  - SP: EL1 uses its own stack pointer (SPSel = 1)
```

That's it. Four registers, a handful of system register bits. Compare this to
naos-linux's boot.rs: no GDT, no page tables, no CR0/CR3/CR4/EFER, no segment
selectors.

### System registers

ARM64 system registers are accessed via dedicated instructions (`MSR`/`MRS`),
not via memory-mapped locations. We set them through HVF's
`hv_vcpu_set_sys_reg`. The ones that matter:

```
SCTLR_EL1 (System Control Register, EL1):
┌──────────────────────────────────────────────────┐
│  Bit 0:  M   = 0  (MMU off — kernel turns it on)│
│  Bit 2:  C   = 1  (D-cache on)                  │
│  Bit 12: I   = 1  (I-cache on)                  │
│  Bit 25: EE  = 0  (little-endian at EL1)         │
│  All other bits: 0 or reset values               │
└──────────────────────────────────────────────────┘

CPACR_EL1 (Coprocessor Access Control Register, EL1):
  FPEN (bits 21:20) = 0b11 — FP/SIMD accessible at EL0 and EL1.
  Without this, the kernel faults on the first FP instruction.

SPSR_EL1 (Saved Program Status Register):
  Not set directly — this is the saved state from the last exception.
  Irrelevant at initial entry.

PSTATE:
  DAIF = 0b1111 — all interrupts masked (Debug, SError, IRQ, FIQ).
  The kernel unmasks interrupts when it's ready.
```

> **Spec reference:** ARM Architecture Reference Manual, Section D13 ("AArch64
> System Registers"). SCTLR_EL1 is in D13.2.114, CPACR_EL1 in D13.2.30.

### The code

```rust
// boot.rs
//
// ARM64 vCPU state setup for kernel entry.
//
// This file is naos-macos's equivalent of naos-linux's boot.rs, and it is
// dramatically simpler. There is no GDT, no segmentation, no page table
// construction. The ARM64 boot protocol specifies:
//   - EL1 with MMU off
//   - x0 = DTB address
//   - PC = kernel entry point
//   - A few system register bits
//
// That's the entire setup. The kernel builds its own page tables and
// enables the MMU itself.

use anyhow::{Context, Result};
use applevisor as hv;

/// Configure the vCPU for ARM64 Linux kernel entry.
///
/// Sets system registers and general registers per the ARM64 boot protocol
/// documented in Documentation/arm64/booting.rst in the kernel source.
///
/// After this function, the vCPU is ready to execute the first instruction
/// of the kernel Image. The kernel will set up its own page tables, enable
/// the MMU, and configure its own stack within the first few hundred
/// instructions.
pub fn configure(vcpu: &hv::Vcpu, entry_addr: u64, dtb_addr: u64) -> Result<()> {
    // --- System registers ---

    // SCTLR_EL1: System Control Register.
    // M=0 (MMU off), C=1 (D-cache on), I=1 (I-cache on), EE=0 (little-endian).
    //
    // The reset value of SCTLR_EL1 varies by implementation. We set it
    // explicitly to avoid depending on HVF's default.
    //
    // Bit 2 (C) = 0x4, Bit 12 (I) = 0x1000
    // Result: 0x1004
    //
    // ARM ARM D13.2.114
    let sctlr_el1: u64 = (1 << 2) | (1 << 12); // C + I, MMU off
    vcpu.set_sys_reg(hv::SysReg::SCTLR_EL1, sctlr_el1)
        .context("Failed to set SCTLR_EL1")?;

    // CPACR_EL1: enable FP/SIMD access.
    // FPEN (bits 21:20) = 0b11 → no trapping of FP/SIMD at EL1 or EL0.
    //
    // Without this, the kernel hits a trap on the first FP instruction
    // (often in memcpy or crypto init) and hangs or panics.
    //
    // ARM ARM D13.2.30
    let cpacr_el1: u64 = 0b11 << 20; // FPEN = 0b11
    vcpu.set_sys_reg(hv::SysReg::CPACR_EL1, cpacr_el1)
        .context("Failed to set CPACR_EL1")?;

    // --- PSTATE ---
    // The kernel expects interrupts to be masked at entry.
    // PSTATE.DAIF = 0b1111 → Debug, SError, IRQ, FIQ all masked.
    // PSTATE.EL = 1 (EL1), PSTATE.SP = 1 (use SP_EL1).
    //
    // PSTATE is set via the SPSR_EL1 register (or via HVF's CPSR accessor,
    // depending on the binding).
    //
    // The exact bit layout:
    //   Bits 9:6 = DAIF = 0b1111 = 0x3C0
    //   Bits 3:2 = EL = 0b01 (EL1) = 0x4
    //   Bit 0 = SP = 1 (SP_EL1) = 0x1
    //   Bit 4 = M[4] = 0 (AArch64 mode)
    //   Result: 0x3C5
    //
    // ARM ARM C5.2.19 (SPSR_EL1)
    let pstate: u64 = 0x3C5; // EL1h with DAIF masked
    vcpu.set_sys_reg(hv::SysReg::SPSR_EL1, pstate)
        .context("Failed to set PSTATE via SPSR_EL1")?;

    // --- General purpose registers ---

    // x0 = physical address of the DTB.
    // The kernel's head.S reads this to find the device tree.
    vcpu.set_reg(hv::Reg::X0, dtb_addr)
        .context("Failed to set x0 (DTB address)")?;

    // x1, x2, x3 = 0 (reserved by the boot protocol).
    vcpu.set_reg(hv::Reg::X1, 0)
        .context("Failed to set x1")?;
    vcpu.set_reg(hv::Reg::X2, 0)
        .context("Failed to set x2")?;
    vcpu.set_reg(hv::Reg::X3, 0)
        .context("Failed to set x3")?;

    // PC = kernel entry point (start of the Image in guest memory).
    vcpu.set_reg(hv::Reg::PC, entry_addr)
        .context("Failed to set PC")?;

    // SP = 0 (the kernel sets up its own stack immediately).
    vcpu.set_reg(hv::Reg::SP, 0)
        .context("Failed to set SP")?;

    Ok(())
}
```

Compare: naos-linux's `boot.rs` is ~150 lines of GDT + page table + control
register setup. naos-macos's `boot.rs` is ~30 lines of register writes. The
complexity that naos-linux spends on boot.rs, naos-macos spends on dtb.rs. The
total is roughly even, but the distribution is very different.

---

## 9. serial.rs — UART emulation

### PL011 vs 16550: the decision

We have two options for serial output:

**Option A: Emulate a PL011 (ARM's native UART).**

- Pros: Idiomatic for ARM64, kernel expects it at `console=ttyAMA0`, the DTB we
  built describes a PL011.
- Cons: No existing Rust crate for PL011 emulation. We'd write it ourselves
  (~100 lines for the MVP subset).

**Option B: Emulate a 16550 over MMIO, reuse `vm-superio::Serial`.**

- Pros: Zero new code for the register state machine. `vm-superio` works on
  macOS.
- Cons: Less idiomatic for ARM. Need to change the DTB to describe a 16550
  (`compatible = "ns16550a"`) instead of a PL011. Kernel needs `console=ttyS0`
  instead of `console=ttyAMA0`.

**Decision: PL011.** The whole point of naos-macos is learning the ARM platform,
and the PL011 is part of that. Writing a minimal PL011 emulation is ~80 lines —
the PL011 register set for output-only serial is simpler than the 16550. The
learning value justifies the code.

### PL011 register map

The PL011 has registers at offsets from its MMIO base address (0x09000000):

```
Offset  Name   Description
──────  ─────  ──────────────────────────────────────────────────
0x000   UARTDR Data Register — write a byte here to transmit
0x018   UARTFR Flag Register — read to check transmit status
0x024   UARTIBRD Integer Baud Rate Divisor (we ignore baud rate)
0x028   UARTFBRD Fractional Baud Rate Divisor
0x02C   UARTLCR_H Line Control Register
0x030   UARTCR Control Register
0x038   UARTIMSC Interrupt Mask Set/Clear
0x044   UARTICR Interrupt Clear Register
```

For output-only MVP, we only care about two:

- **UARTDR (0x000)**: the kernel writes a byte here → we print it to stdout.
- **UARTFR (0x018)**: the kernel reads this to check if the UART is ready. We
  always return "ready" (transmit FIFO empty, not busy).

Everything else gets a sensible default response (zero, or the written value
echoed back).

> **Spec reference:** The PL011 register set is defined in the
> [ARM PrimeCell UART (PL011) Technical Reference Manual](https://developer.arm.com/documentation/ddi0183/latest/).

### The code

```rust
// serial.rs
//
// PL011 UART emulation for naos-macos.
//
// The ARM PrimeCell PL011 is the standard UART on ARM virtual machines.
// This module emulates just enough of it for the kernel to print boot
// messages: UARTDR (data register) for output, UARTFR (flag register)
// to report "transmitter ready."
//
// Unlike naos-linux which reuses vm-superio for the 16550, we write this
// from scratch because:
// 1. The PL011 is a different device with a different register layout.
// 2. The output-only subset is simpler than the 16550.
// 3. Building it ourselves teaches us the ARM UART model.

use std::io::{self, Write};

/// PL011 register offsets from base address.
/// ARM PrimeCell UART (PL011) TRM, Chapter 3.
const UARTDR: u64 = 0x000;     // Data Register
const UARTFR: u64 = 0x018;     // Flag Register
const UARTCR: u64 = 0x030;     // Control Register
const UARTIMSC: u64 = 0x038;   // Interrupt Mask Set/Clear
const UARTICR: u64 = 0x044;    // Interrupt Clear Register

/// Flag Register bits.
/// Bit 4: BUSY — UART is busy transmitting (0 = not busy).
/// Bit 5: RXFE — Receive FIFO empty (1 = empty, no data to read).
/// Bit 7: TXFE — Transmit FIFO empty (1 = empty, ready to accept data).
const FR_TXFE: u32 = 1 << 7;
const FR_RXFE: u32 = 1 << 5;

/// MMIO base and size of the PL011 in guest physical address space.
pub const PL011_BASE: u64 = 0x0900_0000;
pub const PL011_SIZE: u64 = 0x1000;

/// Check whether an MMIO address falls within the PL011 register space.
pub fn is_pl011_addr(addr: u64) -> bool {
    addr >= PL011_BASE && addr < PL011_BASE + PL011_SIZE
}

/// Minimal PL011 state. For the MVP, we only track the control register
/// (to know if the UART is enabled) and emit bytes to stdout.
pub struct Pl011 {
    /// Control register value. The kernel writes this to enable the UART.
    /// Bit 0 = UARTEN (UART enable), Bit 8 = TXE (transmit enable).
    cr: u32,
}

impl Pl011 {
    pub fn new() -> Self {
        Self {
            // Start with UART enabled and TX enabled, matching reset defaults.
            cr: (1 << 0) | (1 << 8), // UARTEN | TXE
        }
    }

    /// Handle an MMIO write to a PL011 register.
    ///
    /// Called from the vCPU run loop when the guest stores to an address
    /// in the PL011 range.
    pub fn write(&mut self, offset: u64, value: u32) {
        match offset {
            UARTDR => {
                // Data Register: the guest is transmitting a character.
                // The low 8 bits are the character; upper bits are error flags
                // on read, ignored on write.
                let ch = (value & 0xFF) as u8;
                let _ = io::stdout().write_all(&[ch]);
                let _ = io::stdout().flush();
            }
            UARTCR => {
                // Control Register: the kernel configures UART enable,
                // transmit enable, receive enable, etc.
                self.cr = value;
            }
            UARTIMSC => {
                // Interrupt mask: the kernel is configuring which interrupts
                // to enable. We don't deliver interrupts for the MVP, so
                // accept the write and ignore it.
            }
            UARTICR => {
                // Interrupt clear: the kernel is acknowledging an interrupt.
                // Nothing to do.
            }
            _ => {
                // All other registers: accept the write silently.
                // The kernel probes various registers during init; we don't
                // need to act on most of them.
            }
        }
    }

    /// Handle an MMIO read from a PL011 register.
    ///
    /// Called from the vCPU run loop when the guest loads from an address
    /// in the PL011 range.
    pub fn read(&self, offset: u64) -> u32 {
        match offset {
            UARTDR => {
                // Data Register read: the guest is trying to receive a byte.
                // We don't have serial input for the MVP. Return 0.
                0
            }
            UARTFR => {
                // Flag Register: report transmit FIFO empty (ready to send)
                // and receive FIFO empty (no incoming data).
                FR_TXFE | FR_RXFE
            }
            UARTCR => {
                // Return the current control register value.
                self.cr
            }
            _ => {
                // Unknown register: return 0. Safe default.
                0
            }
        }
    }
}
```

---

## 10. vcpu.rs — the run loop

### ARM64 vmexit reasons

HVF's vmexit model differs from KVM's. Instead of a tagged enum like
`VcpuExit::IoOut`, HVF provides an exit information structure that describes
what happened. The exit reasons relevant to our MVP:

```
Exit Reason             What Happened                     Our Response
───────────────────     ─────────────────────────────     ─────────────────────
MMIO (data abort)       Guest read/wrote an unmapped      Dispatch to PL011 or
                        address (our UART registers)      return 0 for unknown

HVC (hypervisor call)   Guest executed HVC instruction    Handle PSCI calls:
                        (PSCI power management)           SYSTEM_OFF → clean exit
                                                          SYSTEM_RESET → clean exit

WFI (wait for int)      Guest executed WFI instruction    Re-enter guest (the
                        (idle loop, power saving)         kernel does this often)

Unknown/other           Something we don't handle         Log and bail
```

The big difference from naos-linux's `IoIn`/`IoOut`: ARM has no port I/O, so
serial communication shows up as **MMIO exits** (data aborts to unmapped
addresses), not PIO exits. And the "kernel halted" signal is a **PSCI call** via
HVC, not a `hlt` instruction.

### PSCI: how the kernel halts

PSCI (Power State Coordination Interface) is the ARM standard for power
management in virtualized environments. When the Linux kernel wants to halt or
reboot, it calls PSCI functions via the HVC instruction. The function is
identified by a number in register `x0`:

```
PSCI Function           x0 value        What it means
───────────────         ─────────       ──────────────────────
PSCI_SYSTEM_OFF         0x84000008      Power off the system
PSCI_SYSTEM_RESET       0x84000009      Reboot the system
PSCI_CPU_OFF            0x84000002      Turn off this CPU
PSCI_VERSION            0x84000000      Query PSCI version
```

When we see an HVC exit with `x0 = 0x84000008` (SYSTEM_OFF), the kernel has
halted. This is our clean exit signal — equivalent to naos-linux catching
`VcpuExit::Hlt`.

> **Spec reference:** PSCI is defined in the
> [ARM Power State Coordination Interface specification](https://developer.arm.com/documentation/den0022/latest).
> The function IDs are in Table 1.

### The code

```rust
// vcpu.rs
//
// The vCPU run loop for naos-macos.
//
// Structurally identical to naos-linux's vcpu.rs — a blocking loop that
// runs the vCPU, dispatches exits, and repeats. The exit reasons differ
// because ARM64 has MMIO instead of PIO and PSCI instead of HLT.

use anyhow::{bail, Context, Result};
use applevisor as hv;

use crate::serial::{self, Pl011};

/// PSCI function IDs (SMCCC compliant, 64-bit calling convention).
/// ARM DEN 0022D, Table 1.
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;
const PSCI_VERSION: u64 = 0x8400_0000;

/// PSCI version: 1.0 (major=1, minor=0).
const PSCI_VERSION_1_0: u64 = 0x0001_0000;

/// Run the vCPU until the guest halts (PSCI SYSTEM_OFF) or an error occurs.
pub fn run(vcpu: &hv::Vcpu, uart: &mut Pl011) -> Result<()> {
    loop {
        // Enter guest mode. Blocks until a vmexit occurs.
        vcpu.run()
            .context("hv_vcpu_run failed")?;

        // Inspect the exit reason.
        let exit_info = vcpu.exit_info();

        match exit_info.reason {
            // --- MMIO exit (data abort) ---
            // The guest accessed an address that isn't backed by mapped
            // memory. This is how device register accesses arrive.
            hv::ExitReason::Exception => {
                let syndrome = exit_info.exception.syndrome;

                // Check if this is a data abort (EC = 0b100100 or 0b100101)
                let ec = (syndrome >> 26) & 0x3F;

                if ec == 0x24 || ec == 0x25 {
                    // Data abort. Extract the faulting address and
                    // read/write direction.
                    let far = vcpu.get_sys_reg(hv::SysReg::FAR_EL2)
                        .context("Failed to read FAR_EL2")?;
                    let is_write = (syndrome >> 6) & 1 == 1;
                    let rt = ((syndrome >> 16) & 0x1F) as usize; // destination register
                    let access_size_bits = (syndrome >> 22) & 0x3;
                    let _ = access_size_bits; // we treat all accesses as 32-bit for now

                    if serial::is_pl011_addr(far) {
                        let offset = far - serial::PL011_BASE;

                        if is_write {
                            // Guest is writing to a UART register.
                            let value = vcpu.get_reg(hv::Reg::from_index(rt))
                                .context("Failed to read source register")?;
                            uart.write(offset, value as u32);
                        } else {
                            // Guest is reading from a UART register.
                            let value = uart.read(offset);
                            vcpu.set_reg(hv::Reg::from_index(rt), value as u64)
                                .context("Failed to write destination register")?;
                        }
                    } else {
                        // MMIO to an address we don't emulate (e.g., GIC).
                        // Return 0 for reads, ignore writes.
                        if !is_write {
                            vcpu.set_reg(hv::Reg::from_index(rt), 0)
                                .context("Failed to write zero to dest register")?;
                        }
                    }

                    // Advance PC past the faulting instruction.
                    // ARM64 instructions are always 4 bytes.
                    let pc = vcpu.get_reg(hv::Reg::PC)
                        .context("Failed to read PC")?;
                    vcpu.set_reg(hv::Reg::PC, pc + 4)
                        .context("Failed to advance PC")?;

                } else if ec == 0x16 {
                    // HVC (Hypervisor Call). Used for PSCI.
                    let x0 = vcpu.get_reg(hv::Reg::X0)
                        .context("Failed to read x0 for PSCI")?;

                    match x0 {
                        PSCI_SYSTEM_OFF => {
                            // Kernel is shutting down. Clean exit.
                            break;
                        }
                        PSCI_SYSTEM_RESET => {
                            // Kernel wants to reboot. For the MVP, treat
                            // as shutdown — we don't implement reboot.
                            break;
                        }
                        PSCI_VERSION => {
                            // Return PSCI version 1.0.
                            vcpu.set_reg(hv::Reg::X0, PSCI_VERSION_1_0)
                                .context("Failed to set PSCI version response")?;
                        }
                        _ => {
                            // Unknown PSCI function. Return NOT_SUPPORTED (-1).
                            vcpu.set_reg(hv::Reg::X0, u64::MAX) // -1 in two's complement
                                .context("Failed to set PSCI error response")?;
                        }
                    }

                    // Advance PC past the HVC instruction.
                    let pc = vcpu.get_reg(hv::Reg::PC)
                        .context("Failed to read PC")?;
                    vcpu.set_reg(hv::Reg::PC, pc + 4)
                        .context("Failed to advance PC past HVC")?;

                } else {
                    bail!("Unhandled exception class: EC=0x{:02x}, syndrome=0x{:08x}", ec, syndrome);
                }
            }

            // --- WFI/WFE ---
            // The guest executed WFI (Wait For Interrupt) or WFE (Wait For Event).
            // This is normal idle behavior — the kernel's idle loop executes WFI.
            // Unlike x86 HLT which we use as a halt signal, WFI on ARM is routine
            // and we just re-enter the guest.
            //
            // The kernel halts via PSCI SYSTEM_OFF, not WFI.
            hv::ExitReason::WFI => {
                // Just re-enter. The guest will resume when the timer fires
                // or an interrupt arrives (which won't happen in the MVP,
                // but the kernel's panic timeout uses the timer).
            }

            // --- Everything else ---
            exit => {
                bail!("Unexpected vCPU exit: {:?}", exit);
            }
        }
    }

    Ok(())
}
```

> **Major open question:** The exact HVF exit reason types and the syndrome
> parsing logic above are based on the ARM Architecture Reference Manual and
> reference HVF implementations (QEMU's HVF backend, UTM). The `applevisor`
> crate may expose these differently — the syndrome might be pre-parsed, the
> exit reason enum might have different variant names, MMIO exits might have
> dedicated handling rather than raw syndrome parsing. **This is the file most
> likely to need significant revision at coding time.** The concepts (MMIO
> dispatch, PSCI handling, WFI pass-through, PC advancement) are correct; the
> exact API calls will need adjustment.

---

## 11. vmm.rs — tying it together

```rust
// vmm.rs
//
// The Vmm struct for naos-macos: orchestrates initialization and owns
// all VMM resources.
//
// The initialization order is simpler than naos-linux because HVF has
// fewer ordering constraints. The main sequence:
//   1. Create VM (hv_vm_create)
//   2. Allocate and map guest memory
//   3. Build and write DTB into guest memory
//   4. Load kernel Image into guest memory
//   5. Create and configure vCPU
//   6. Run

use std::path::Path;

use anyhow::{Context, Result};
use applevisor as hv;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use crate::{addr, boot, dtb, kernel, serial, vcpu};

pub struct Vmm {
    vcpu: hv::Vcpu,
    uart: serial::Pl011,
    // Guest memory is kept alive for the lifetime of the VM.
    // HVF references the mmap'd region; dropping it would unmap guest RAM.
    _guest_mem: GuestMemoryMmap,
}

impl Vmm {
    pub fn new(kernel_path: &Path, mem_mib: u64, cmdline: &str) -> Result<Self> {
        let mem_bytes = mem_mib * 1024 * 1024;

        // --- Step 1: Create VM ---
        // hv_vm_create is a process-global operation. There can be only
        // one VM per process. This is a Hypervisor.framework constraint.
        hv::vm_create(None)
            .context("hv_vm_create failed — is Hypervisor.framework available?")?;

        // --- Step 2: Allocate and map guest memory ---
        let guest_mem = crate::memory::build(mem_mib)?;
        crate::memory::register(&guest_mem)?;

        // --- Step 3: Build and write the device tree ---
        let dtb_blob = dtb::build(mem_bytes, cmdline)?;

        // Write the DTB at the beginning of RAM (DTB_ADDR = RAM_BASE).
        guest_mem
            .write_slice(&dtb_blob, GuestAddress(addr::DTB_ADDR))
            .context("Failed to write DTB to guest memory")?;

        // --- Step 4: Load the kernel ---
        let entry_addr = kernel::load(&guest_mem, kernel_path)?;

        // --- Step 5: Create and configure vCPU ---
        let vcpu = hv::Vcpu::new()
            .context("Failed to create vCPU")?;

        boot::configure(&vcpu, entry_addr, addr::DTB_ADDR)?;

        // --- Step 6: Create UART ---
        let uart = serial::Pl011::new();

        Ok(Self {
            vcpu,
            uart,
            _guest_mem: guest_mem,
        })
    }

    pub fn run(&mut self) -> Result<()> {
        vcpu::run(&self.vcpu, &mut self.uart)
    }
}

impl Drop for Vmm {
    fn drop(&mut self) {
        // Clean up HVF resources. Order matters: destroy vCPU before VM.
        // Errors during cleanup are logged but not propagated.
        let _ = self.vcpu.destroy();
        let _ = hv::vm_destroy();
    }
}
```

---

## 12. main.rs — entry point

```rust
// main.rs
//
// CLI entry point for naos-macos. Structurally identical to naos-linux's
// main.rs — three arguments, build the VMM, run it.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

mod addr;
mod boot;
mod dtb;
mod kernel;
mod memory;
mod serial;
mod vcpu;
mod vmm;

/// naos-macos: minimum viable Hypervisor.framework-based hypervisor.
///
/// Boots an aarch64 Linux kernel Image via Hypervisor.framework and prints
/// its output to stdout. The kernel will panic when it cannot find an init
/// process — that panic is the success signal.
#[derive(Parser, Debug)]
#[command(name = "naos-macos")]
struct Args {
    /// Path to an aarch64 Linux kernel Image file.
    #[arg(long)]
    kernel: PathBuf,

    /// Guest RAM in MiB.
    #[arg(long, default_value_t = 256)]
    mem: u64,

    /// Kernel command line.
    #[arg(long, default_value = "console=ttyAMA0 reboot=k panic=1")]
    cmdline: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let mut vmm = vmm::Vmm::new(&args.kernel, args.mem, &args.cmdline)?;

    vmm.run()
}
```

---

## 13. Running it

### Prerequisites

1. An Apple Silicon Mac (M1 or later) running macOS 11+.
2. Hypervisor.framework entitlement. The binary must be signed with the
   `com.apple.security.hypervisor` entitlement, or your user must have SIP
   (System Integrity Protection) configured to allow it. During development,
   running unsigned binaries may require a specific entitlement plist — this
   will be documented in DEVELOPMENT.md when stage 2 begins.
3. An aarch64 test kernel built from source. Instructions TBD in DEVELOPMENT.md.

### Build and run

```bash
cd ~/code/naos
just build       # on macOS, builds naos-macos
just run --kernel testdata/aarch64/Image --mem 256
```

### Expected output

```
[    0.000000] Booting Linux on physical CPU 0x0000000000 [0x00000000]
[    0.000000] Linux version 6.12.0 ...
[    0.000000] Machine model: naos virtual machine
[    0.000000] Command line: console=ttyAMA0 reboot=k panic=1
[    0.000000] Memory: 256MB ...
...
[    0.xxxxxx] Kernel panic - not syncing: No working init found.
```

---

## 14. What happens during boot

Step-by-step trace, paralleling naos-linux's section 12:

1. **`main()` parses args**, hands them to `Vmm::new()`.
2. **`hv_vm_create` creates the Hypervisor.framework VM context.** This is
   process-global — one VM per process.
3. **`memory::build(256)` allocates 256 MiB** via `mmap(2)`.
   `memory::register()` calls `hv_vm_map` to make it visible at guest physical
   address 0x40000000.
4. **`dtb::build()` constructs a Flattened Device Tree** describing one CPU, 256
   MiB of RAM at 0x40000000, a PL011 UART at 0x09000000, a GIC at 0x08000000, a
   timer, and a PSCI node. The DTB is written to guest memory at 0x40000000 (the
   start of RAM).
5. **`kernel::load()` reads the kernel Image**, validates the ARM64 magic,
   copies it into guest memory at 0x40080000 (RAM_BASE + 0x80000).
6. **`boot::configure()` sets the vCPU state**: SCTLR_EL1 with MMU off, caches
   on; CPACR_EL1 with FP enabled; PSTATE at EL1h with interrupts masked; x0 =
   DTB address (0x40000000); PC = kernel entry (0x40080000).
7. **`vmm.run()` enters the main loop.** First `hv_vcpu_run` enters guest mode.
8. **The kernel's `head.S`** starts executing at the Image's first instruction
   (a branch past the header). It reads x0 to find the DTB, parses the `/memory`
   node to discover RAM, and sets up its own page tables.
9. **The kernel enables the MMU** via SCTLR_EL1.M=1. From this point, guest
   virtual addresses go through the kernel's page tables (not our identity
   mapping, which doesn't exist — we started with MMU off).
10. **The kernel parses the DTB** to discover all hardware: the PL011 UART, the
    GIC, the timer. It initializes drivers for each.
11. **Every `printk()` writes to the PL011** at 0x09000000 via an MMIO store.
    This address isn't backed by mapped memory, so it causes a data abort
    vmexit. naos-macos inspects the faulting address, recognizes it as PL011,
    extracts the written byte, and prints it to stdout.
12. **The kernel reads UARTFR** (offset 0x018 from PL011 base) before each write
    to check transmit readiness. Our `Pl011::read` returns `FR_TXFE | FR_RXFE` —
    "transmitter empty, ready to go."
13. **After initialization, the kernel tries to run `/init`**, fails, prints "No
    working init found."
14. **`panic=1` triggers a reboot after 1 second.** The kernel calls
    `machine_restart()`, which invokes PSCI `SYSTEM_RESET` via an HVC
    instruction.
15. **The HVC instruction causes a vmexit.** naos-macos reads x0 = `0x84000009`
    (PSCI_SYSTEM_RESET), recognizes it as a shutdown signal, and breaks out of
    the run loop.
16. **`Vmm::drop` calls `hv_vcpu_destroy` and `hv_vm_destroy`.** `main()` exits
    with status 0.

---

## 15. What's next

The naos-macos MVP produces the same output as naos-linux — kernel dmesg
followed by a clean exit on panic — but through a completely different
hypervisor API, a different CPU architecture, and a different boot protocol.
With both MVPs complete, the next milestone is:

**Stage 3: naos-vmm.** The trait abstraction extracted from what naos-linux and
naos-macos actually share. By this point you'll have written both run loops,
both memory modules, both boot modules, and both serial modules. The shape of
the trait — what's common, what's backend-specific, where the seam is — will be
obvious from the code, not from speculation.

Looking at the two walkthroughs side by side, the likely trait boundaries are:

- **`Hypervisor` trait**: `create_vm`, `map_memory`, `create_vcpu` —
  structurally identical across backends, different API calls.
- **`Vcpu` trait**: `set_reg`, `set_sys_reg`, `run`, `exit_info` — the run loop
  shape is identical (loop, match exit, dispatch, re-enter), but the exit reason
  types and register access APIs differ.
- **Shared, no trait needed**: `vm-memory` (already cross-platform), `clap`
  args, error handling, the overall VMM struct shape.
- **Backend-specific, no trait possible**: boot.rs (GDT vs DTB is fundamentally
  different), serial.rs (16550 PIO vs PL011 MMIO), kernel.rs (ELF vs Image).

The abstraction is smaller than it looks from the outside. Most of the VMM's
complexity is in arch-specific boot and device emulation, which doesn't
abstract. The trait covers maybe 50 lines of interface. That's a good sign — it
means the abstraction is honest and narrow.

---

## Reference links

- **ARM Architecture Reference Manual (ARM ARM)**:
  [developer.arm.com/documentation/ddi0487/latest](https://developer.arm.com/documentation/ddi0487/latest)
  — system registers, exception levels, page tables, instructions.
- **ARM64 Linux boot protocol**:
  [Documentation/arm64/booting.rst](https://www.kernel.org/doc/html/latest/arch/arm64/booting.html)
  — Image header, register state at entry.
- **Hypervisor.framework**:
  [developer.apple.com/documentation/hypervisor](https://developer.apple.com/documentation/hypervisor)
  — Apple's hypervisor API reference.
- **PSCI specification**:
  [ARM DEN 0022](https://developer.arm.com/documentation/den0022/latest) — power
  management function IDs and calling conventions.
- **PL011 TRM**:
  [developer.arm.com/documentation/ddi0183](https://developer.arm.com/documentation/ddi0183/latest/)
  — UART register map and behavior.
- **Device Tree specification**:
  [devicetree.org/specifications](https://www.devicetree.org/specifications/) —
  FDT format, node/property structure.
- **QEMU virt machine source**:
  [hw/arm/virt.c](https://github.com/qemu/qemu/blob/master/hw/arm/virt.c) — the
  reference address map for ARM64 VMs.
- **vm-fdt crate**: [docs.rs/vm-fdt](https://docs.rs/vm-fdt) — device tree
  construction API.
- **applevisor crate**: [docs.rs/applevisor](https://docs.rs/applevisor) —
  Hypervisor.framework Rust bindings (verify this is still the right crate at
  coding time).
