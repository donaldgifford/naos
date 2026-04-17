# naos-linux minimum viable hypervisor

This document describes the first stage of naos: a minimum viable hypervisor for
Linux hosts with KVM and x86_64 guests. It is scoped strictly to the
implementation of the `naos-linux` crate and does not cover naos as a whole —
see `ARCHITECTURE.md` at the repository root for the project-level context, the
philosophy, and the relationship between this crate and the rest of the planned
platform.

## Overview

`naos-linux` is the first concrete implementation of a naos hypervisor backend.
It runs on x86_64 Linux hosts with KVM enabled, targets x86_64 Linux guests, and
is built directly on the rust-vmm crate ecosystem without any abstraction layer
between it and the hypervisor primitives. This document defines its minimum
viable scope: the irreducible core needed to run a Linux kernel under KVM and
observe its boot output, and nothing more.

The MVP exists to establish the architectural skeleton of a KVM-based VMM — KVM
handle, guest memory, vCPU loop, kernel loading, boot state, one device —
without entangling it with virtio or any other subsystem. Each subsystem will be
its own design doc when its time comes. By stopping at "kernel boots and panics
looking for init," the MVP exercises every load-bearing piece of a hypervisor
exactly once and nothing more.

**Acceptance criteria.** Running `naos-linux --kernel <vmlinux> --mem 256` loads
a Linux vmlinux ELF, transitions a single vCPU into 64-bit long mode at the
kernel entry point, and prints kernel boot messages to the host's stdout via an
emulated 16550 UART. The kernel will panic when it cannot find an init process;
naos-linux detects the resulting halt, exits cleanly, and returns zero. Success
is dmesg on stdout, not a shell.

## Non-goals

The following are intentionally excluded from the MVP. Each will be addressed in
a later design doc when it becomes load-bearing; none are permanent exclusions.

- **Rootfs and block devices.** No virtio-blk, no disk image, no filesystem. The
  kernel panics on missing init and that is the success signal.
- **Networking.** No virtio-net, no tap device, no host bridging.
- **Serial input.** The UART is output-only; stdin is not wired to the guest.
  This avoids needing an event loop.
- **Multiple vCPUs.** Single vCPU on a single thread. No SMP, no MPTable, no
  ACPI MADT.
- **MMIO bus and device abstractions.** No `vm-device`, no `vm-allocator`. The
  single PIO device (UART) is wired directly.
- **bzImage support.** vmlinux ELF only. bzImage requires the real-mode
  trampoline and zero page setup, which are deferred.
- **Snapshot, jailer, API socket, metrics, vsock, balloon, virtio-fs.** All
  platform features are post-MVP.
- **Any abstraction layer.** naos-linux targets KVM directly via `kvm-ioctls`.
  It does not implement a trait, does not anticipate the future `naos-vmm`
  abstraction, and does not share code with `naos-macos`. The abstraction will
  be extracted later from both concrete implementations — see `ARCHITECTURE.md`.
- **Firecracker microVM compatibility and full Linux distro support.** Later
  stages, tracked separately.

## Interface

Single binary, single command, three arguments:

```
naos-linux --kernel <PATH> --mem <MIB> [--cmdline <STRING>]
```

- `--kernel` — path to a vmlinux ELF file. Required.
- `--mem` — guest RAM in MiB. Defaults to 256.
- `--cmdline` — kernel command line. Defaults to
  `console=ttyS0 reboot=k panic=1 pci=off`.

Output: kernel boot messages on stdout, naos-linux diagnostics on stderr. Exit
code 0 on clean halt, non-zero on unexpected vCPU exit or setup failure.

## Implementation

### Crate dependencies

In scope for MVP:

- `kvm-ioctls`, `kvm-bindings` — KVM API wrappers
- `vm-memory` — guest memory abstraction over mmap
- `linux-loader` — vmlinux ELF parsing and loading
- `vm-superio` — 16550 UART emulation
- `vmm-sys-util` — eventfd and assorted Linux primitives
- `anyhow` — error propagation
- `clap` — argument parsing

Deliberately excluded: `event-manager`, `vm-allocator`, `vm-device`, all
`virtio-*` crates. Each will earn its place when a second device or a second
thread forces the issue.

### Crate layout

`naos-linux` is a single crate within the naos workspace. Every file targets
under ~150 lines. `boot.rs` is the only file that earns real complexity and
receives the most comments — it is the only file where the _why_ is not obvious
from the _what_, and it should be auditable against the Intel SDM or AMD APM by
someone who has never touched the code before.

```
crates/naos-linux/
├── Cargo.toml
└── src/
    ├── main.rs     # arg parsing, build Vmm, run, error handling
    ├── vmm.rs      # Vmm struct: owns kvm, vm, memory, vcpu, serial
    ├── memory.rs   # build GuestMemoryMmap, register region with KVM
    ├── kernel.rs   # linux-loader wrapper, returns entry address
    ├── boot.rs     # GDT, page tables, sregs, regs (heavily commented)
    ├── vcpu.rs     # run loop and exit dispatch
    └── serial.rs   # vm-superio wiring with stdout sink
```

### The six pieces

**1. KVM handle and VM fd (`vmm.rs`).** Open `/dev/kvm` via `Kvm::new`. Verify
API version. Create the VM with `kvm.create_vm`. Set the TSS address (required
on Intel before creating vCPUs; a no-op on AMD but called unconditionally).
Create the in-kernel IRQ chip with `vm.create_irq_chip`. The IRQ chip is not
strictly necessary for the MVP since no interrupts will fire, but creating it
now costs nothing and avoids a footnote later.

naos-linux runs on any x86_64 host with KVM, Intel VT-x or AMD SVM. KVM
abstracts the vendor difference for everything we touch — memory regions, vCPU
creation, sregs/regs, and the in-kernel IRQ chip work identically on both. The
boot state configured in `boot.rs` is x86_64 architectural state defined by
AMD64 and adopted by Intel, so a vmlinux boots the same way on either vendor.
Vendor-specific concerns (SEV/SEV-SNP, TDX, nested virt quirks, vendor CPUID
masking) are all post-MVP.

**2. Guest memory (`memory.rs`).** One anonymous mmap region of `--mem` MiB at
guest physical address 0, built with `GuestMemoryMmap::from_ranges`. Register
the region with KVM via `set_user_memory_region`. Single region, no MMIO hole —
we have no MMIO devices yet, so the address space is contiguous RAM.

**3. Kernel loader (`kernel.rs`).** Wrap `linux_loader::loader::elf::Elf::load`
against the guest memory. vmlinux ELF only — no bzImage path. Returns the kernel
entry address as a `GuestAddress`. The kernel command line is written separately
into guest memory at a fixed low address; for the MVP we pass the cmdline
pointer in `RSI` per the Linux x86_64 boot protocol, but with no zero page (the
kernel tolerates this for ELF entry as long as the cmdline is reachable).

**4. Boot state (`boot.rs`).** The only file that earns its complexity. Manually
constructs:

- A 3-entry GDT (null descriptor, 64-bit code segment, data segment) written
  into guest memory at a fixed low address.
- 4-level identity-mapped page tables covering the first 1 GiB: one PML4, one
  PDPT, one PD using 2 MiB large pages. 1 GiB is more than enough headroom for a
  256 MiB guest and avoids needing PT entries entirely.
- Special registers (`sregs`): `CR0` with PE and PG set, `CR4` with PAE set,
  `EFER` with LME and LMA set, `CR3` pointing at the PML4, and segment registers
  (CS, DS, ES, FS, GS, SS) loaded with the GDT selectors so the vCPU starts in
  64-bit long mode.
- General registers (`regs`): `RIP` at the kernel entry address, `RSI` at the
  cmdline address, the rest zeroed.

Every constant in this file is annotated with the section it comes from in the
Intel SDM Vol. 3 or AMD APM Vol. 2 — both manuals document the same x86_64
architectural state and either is a valid audit reference. This is the file most
likely to break in subtle ways and the file most likely to be audited by
future-us.

**5. 16550 UART (`serial.rs`).** Use `vm_superio::Serial` with stdout as the
output sink. Wire it to PIO ports `0x3F8`–`0x3FF`. Output only — no stdin, no
event loop. The kernel will write to this port during early boot and we forward
bytes directly to `io::stdout`.

**6. vCPU run loop (`vcpu.rs`).** Single thread, blocking `vcpu.run()` in a
loop. Match on `VcpuExit`:

- `IoOut(port, data)` where port is in the serial range →
  `serial.write(port - 0x3F8, data)`
- `IoIn(port, data)` where port is in the serial range →
  `serial.read(port - 0x3F8, data)`
- `Hlt` → break the loop, return `Ok(())`
- Anything else → log the exit variant and return an error

That dispatch table is the entire MVP control flow.

### Boot sequence

```
main
 └─ parse args (kernel path, mem MiB, cmdline)
 └─ Vmm::new
     ├─ Kvm::new                            # open /dev/kvm
     ├─ kvm.create_vm                       # VM fd
     ├─ memory::build(mem_mib)              # mmap + register region
     ├─ kernel::load(&mem, path)            # returns entry address
     ├─ write cmdline into guest memory     # at known low address
     ├─ vm.set_tss_address
     ├─ vm.create_irq_chip                  # PIC + IOAPIC in-kernel
     ├─ serial::new()                       # owns 16550 state, stdout sink
     ├─ vm.create_vcpu(0)
     └─ boot::configure(&vcpu, &mem, entry, cmdline_addr)
         ├─ write GDT into guest memory
         ├─ write PML4 / PDPT / PD (identity 1 GiB, 2 MiB pages)
         ├─ sregs: CR0.PE|PG, CR4.PAE, EFER.LME|LMA, CS/DS/ES/FS/GS/SS
         └─ regs: rip = entry, rsi = cmdline_addr
 └─ vmm.run
     └─ loop {
            match vcpu.run()? {
                IoOut(0x3F8..=0x3FF, d) => serial.write(...),
                IoIn (0x3F8..=0x3FF, b) => serial.read(...),
                Hlt                     => break,
                other                   => bail!("unexpected exit: {other:?}"),
            }
        }
```

## Error handling

All fallible operations return `anyhow::Result`. Errors propagate to `main`,
which prints the error chain to stderr and exits with a non-zero code. There is
no recovery logic in the MVP — every error is fatal because the VMM is
single-purpose and stateless.

Expected error categories:

- **Setup failures** (KVM unavailable, kernel file missing, ELF parse failure,
  memory mmap failure) — surfaced before the vCPU runs, with context describing
  which step failed.
- **Unexpected vCPU exits** (anything other than serial PIO or Hlt) — surfaced
  with the exit variant logged. These represent gaps in our emulation surface
  and are bugs to fix, not conditions to handle.
- **Clean halt** — not an error. The vCPU executing `hlt` after the kernel
  panics is the success path.

## Testing strategy

The MVP is small enough that the integration test _is_ the unit test: does
`cargo run -p naos-linux -- --kernel testdata/vmlinux --mem 256` produce kernel
boot messages and exit cleanly? That single end-to-end check exercises every
component.

Unit tests where they pay for themselves:

- `boot.rs` — verify GDT layout bytes match the SDM-specified format, verify
  page table entries point where we think they point. These are the constants
  most likely to be wrong and the hardest to debug at runtime.
- `memory.rs` — verify region registration succeeds with valid sizes and fails
  predictably with invalid ones.

Out of scope for MVP testing: any kind of fuzzing, property tests, multi-vCPU
race testing, or KVM mock layer. We will revisit when there is enough surface
area to justify the harness.

Manual test artifact: a known-good vmlinux built from a recent stable kernel
with `tinyconfig` plus serial console support. Build instructions live in
`DEVELOPMENT.md`.

## Open questions

- Exact guest memory addresses for GDT, page tables, and cmdline. Firecracker's
  choices are reasonable defaults but worth confirming against the SDM and the
  linux-loader conventions before writing `boot.rs`.
- Whether to expose the binary name as `naos-linux` or alias it to `naos` when
  only one backend is installed. Cosmetic, decide when wiring `Cargo.toml`.
- Source of the test vmlinux — build script, committed binary, or
  fetch-on-demand. Covered in `DEVELOPMENT.md`.
