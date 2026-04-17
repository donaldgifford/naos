# naos-macos minimum viable hypervisor

This document describes the second stage of naos: a minimum viable hypervisor
for macOS hosts with Hypervisor.framework and aarch64 Linux guests. It parallels
`DESIGN-naos-linux.md` in philosophy and scope but targets a different
hypervisor API and a different guest architecture. See `ARCHITECTURE.md` at the
repository root for the project-level context.

This is a design sketch written before implementation work begins. Some
decisions are already made; others are marked as open questions to be resolved
during or after stage 1 (`naos-linux`) is complete. The level of detail is
deliberately lower than `DESIGN-naos-linux.md` — the philosophy is set, the
shape is clear, but many of the specifics will only become clear once the author
has hands on the Hypervisor.framework API and has read the ARM Architecture
Reference Manual in anger. This document exists to capture the shape; it will be
revised when work actually starts.

## Overview

`naos-macos` is the second concrete implementation of a naos hypervisor backend.
It runs on macOS hosts with Hypervisor.framework enabled, targets aarch64 Linux
guests (on Apple Silicon hosts), and is built directly against
Hypervisor.framework via Rust bindings without any abstraction layer between it
and the hypervisor primitives.

Like `naos-linux`, it is scoped to the irreducible core needed to run a Linux
kernel and observe its boot output. The philosophy is identical: minimum viable
everything, comments over abstraction, concrete before abstract, no dependencies
we would not write ourselves, opinions not options.

**Acceptance criteria.** Running `naos-macos --kernel <Image> --mem 256` on an
Apple Silicon Mac loads an aarch64 Linux kernel image, transitions a single vCPU
into EL1 at the kernel entry point, and prints kernel boot messages to the
host's stdout via an emulated 16550 UART (or PL011 — see open questions). The
kernel panics on missing init, naos-macos detects the halt, exits cleanly, and
returns zero. Success is dmesg on stdout.

## Why this stage exists

`naos-linux` teaches x86_64 VMM internals and the KVM API. `naos-macos` teaches
aarch64 VMM internals and the Hypervisor.framework API. These are two largely
disjoint bodies of knowledge, and building both is the point. A few specific
reasons this stage is worth the effort:

- **A faster feedback loop on the Mac.** Stage 1 requires SSHing to the NixOS
  workstation for every `cargo run`. Stage 2 lets the author run VMs directly on
  the MacBook they type on every day, which is a meaningful workflow improvement
  once the VMM exists.
- **A second concrete implementation is the prerequisite for stage 3.** The
  abstraction layer (`naos-vmm`) cannot be designed honestly from one example.
  Building `naos-macos` is what makes `naos-vmm` possible later.
- **aarch64 is architecturally cleaner than x86_64 in several ways.** No
  segmentation, no long-mode dance, no GDT, no real-mode legacy. Building a
  second VMM is not twice the work of building the first, especially on the
  boot-path side, where `naos-macos`'s equivalent of `boot.rs` should be
  noticeably simpler than `naos-linux`'s.
- **Hypervisor.framework is first-party, stable, and well-documented.** Apple
  maintains it, ships it on every Mac, and exposes it via both a C API and a
  higher-level Swift framework. It is a genuinely good API, and engaging with it
  directly is worth doing once.

## Non-goals

Mirrors `naos-linux` non-goals, adjusted for the stage:

- **Rootfs, networking, serial input, multiple vCPUs, MMIO bus, snapshot,
  jailer, API socket.** All post-MVP, same as stage 1.
- **x86_64 guests on Apple Silicon.** Impossible without full software CPU
  emulation (QEMU TCG). Out of scope forever for this crate.
- **Intel Mac support.** Hypervisor.framework on Intel Macs would target x86_64
  guests, which is a genuinely different code path — closer to `naos-linux` in
  guest architecture but to `naos-macos` in host API. The author does not own an
  Intel Mac and cannot meaningfully test this. If someone who does want to build
  it, it should probably be a third crate (`naos-macos-intel`?) rather than
  cfg-gating inside `naos-macos`.
- **Sharing code with `naos-linux`.** The two crates are deliberately
  independent. Duplication is expected and acceptable. The abstraction comes in
  stage 3, not now.
- **Universal binary.** `naos-macos` targets Apple Silicon only. aarch64 only.

## Interface

Single binary, single command, three arguments, parallel to `naos-linux`:

```
naos-macos --kernel <PATH> --mem <MIB> [--cmdline <STRING>]
```

- `--kernel` — path to an aarch64 Linux kernel image. Format TBD (see open
  questions).
- `--mem` — guest RAM in MiB. Defaults to 256.
- `--cmdline` — kernel command line. Default TBD once the chosen serial device
  is wired up.

Output: kernel boot messages on stdout, naos-macos diagnostics on stderr. Exit
code 0 on clean halt, non-zero on unexpected vCPU exit or setup failure.

## Implementation

### Crate dependencies

In scope:

- **A Hypervisor.framework Rust binding.** Candidates: `ahv`, `applevisor`, or
  hand-rolled bindings via `objc2` / direct FFI. Decision deferred until stage 2
  starts — we want to know which one is actively maintained and has the right
  API shape at the time, not what was best a year before we used it.
- `vm-memory` — if its abstractions hold up cross-platform. The crate is pure
  Rust with no Linux-specific code at its core, but this needs verification. If
  it does not work, we write a minimal guest memory abstraction ourselves
  (likely ~50 lines).
- `vm-superio` — for the 16550 UART emulation. Pure logic, should work anywhere.
  Alternative: PL011 emulation, which is more idiomatic on aarch64 Linux but may
  not have a prebuilt crate.
- `linux-loader` — if its aarch64 kernel loading path works. The crate claims to
  support aarch64, worth verifying before committing.
- `anyhow`, `clap` — same as stage 1.

Out of scope: anything KVM-related. `kvm-ioctls` and `kvm-bindings` are
Linux-only and will not compile on macOS regardless of architecture.

### Crate layout

Parallel to `naos-linux` but with different internals where the architecture
differs.

```
crates/naos-macos/
├── Cargo.toml
└── src/
    ├── main.rs     # arg parsing, build Vmm, run, error handling
    ├── vmm.rs      # Vmm struct: owns hv context, memory, vcpu, serial
    ├── memory.rs   # guest memory via hv_vm_map
    ├── kernel.rs   # aarch64 kernel loader — format TBD
    ├── boot.rs     # aarch64 register setup (simpler than x86_64!)
    ├── vcpu.rs     # run loop and exit dispatch
    └── serial.rs   # UART emulation wired to stdout
```

Same aesthetic constraints as `naos-linux`: each file under ~150 lines,
`boot.rs` is the only file that earns real complexity, all comments reference
the ARM Architecture Reference Manual where constants come from it.

### The six pieces (aarch64 / HVF version)

**1. Hypervisor.framework handle and VM (`vmm.rs`).** Create a
Hypervisor.framework VM context via the chosen binding. The Apple Silicon flavor
of Hypervisor.framework is newer and has a somewhat different API from the Intel
flavor — vCPUs are created with `hv_vcpu_create`, configured via
`hv_vcpu_set_sys_reg` / `hv_vcpu_set_reg`, and run with `hv_vcpu_run`. Exits are
surfaced as a struct describing the reason rather than as a tagged union. The
binding crate will smooth most of this over, but the underlying model is: create
VM, map memory, create vCPU, set registers, run.

**2. Guest memory (`memory.rs`).** Allocate a large mmap region in the host
process, then call `hv_vm_map` to make it guest-visible at a specific guest
physical address with read/write/execute permissions. Unlike KVM, the memory
region on Hypervisor.framework is identified by a host virtual address pointer
rather than a file descriptor and offset, which simplifies some things and
complicates others. Single region, no MMIO hole for the MVP, same as stage 1.

On aarch64, guest physical address 0 is not necessarily where the kernel expects
to live — the ARM64 Linux boot protocol specifies that the kernel Image should
be loaded at `TEXT_OFFSET` (typically 0x80000) above the base of DRAM. This is a
real concern and will be worked out when writing `memory.rs` and `kernel.rs`.

**3. Kernel loader (`kernel.rs`).** aarch64 Linux kernels are typically
distributed as a flat `Image` file (the decompressed kernel binary with a small
header), not as an ELF. The boot protocol is documented in the Linux kernel tree
at `Documentation/arm64/booting.rst` and is notably simpler than the x86_64
protocol: load the `Image` at the right offset, set `x0` to a device tree blob
address, and jump to the start of the image. No zero page, no real-mode
trampoline, no long-mode dance.

The kernel loader needs to:

- Parse the 64-byte Image header (magic, text_offset, image_size, flags).
- Copy the image into guest memory at the right offset.
- Return the entry address.

This is maybe 40 lines of code. `linux-loader` may do it for us; worth checking
when stage 2 starts.

**4. Boot state (`boot.rs`).** The aarch64 boot protocol expects:

- The CPU in EL1 with MMU off, caches on, interrupts off.
- `x0` = physical address of a device tree blob (DTB) in memory.
- `x1` = 0 (reserved).
- `x2` = 0 (reserved).
- `x3` = 0 (reserved).
- PC = kernel entry address.

No GDT, no page tables, no long-mode transition. The entire boot state setup is
"set a few system registers, put a DTB pointer in x0, set PC, run."

The catch: we need a device tree blob. Linux on aarch64 does not use ACPI by
default (though it can); it uses a device tree to describe the hardware. For the
MVP, we need a minimal DTB describing:

- A single CPU (the ARM generic timer, PSCI for power management).
- Memory (base and size).
- A UART device (16550 or PL011 — open question).
- The chosen interrupt controller (GIC — open question on which version).

Building a minimal DTB is its own sub-project and may deserve its own small
crate. Options:

- Use a prebuilt DTB checked into the repo as test data.
- Build a DTB programmatically at runtime using a crate like `vm-fdt`.
- Hand-write a DTS file, compile it to DTB at build time via `dtc`, and include
  the result.

This is the single biggest unknown in stage 2, and it will likely dominate the
stage 2 work. The plan is to resolve it when we get there, not now.

**5. UART (`serial.rs`).** aarch64 Linux systems traditionally use a PL011 UART,
not a 16550. We have two options:

- Emulate a 16550 (reuse `vm-superio::Serial` from stage 1) and tell the kernel
  about it via the device tree. aarch64 Linux supports 16550 over MMIO.
- Emulate a PL011, which is more idiomatic but requires us to write or find a
  PL011 emulation library.

For the MVP, reusing `vm-superio::Serial` is the right call if it works — code
reuse is not premature abstraction when it is this concrete. PL011 can come
later if it becomes motivated.

**6. vCPU run loop (`vcpu.rs`).** Single thread, blocking `hv_vcpu_run` in a
loop. Match on the exit reason and dispatch:

- **MMIO read/write in the UART range** → serial emulation.
- **HVC or SMC call** → if it is a PSCI call to power off or reset, clean exit;
  otherwise bail.
- **WFI / WFE** → the aarch64 analog of `HLT`. Probably should not break the
  loop on these (the kernel WFIs all the time during idle); instead, just
  re-enter `hv_vcpu_run`. The MVP signal for "kernel panicked and halted" is
  more likely to be a PSCI `SYSTEM_OFF` call or an infinite WFI loop with no
  interrupts to wake it.
- **Anything else** → log and bail.

The exit dispatch on aarch64 is meaningfully different from x86_64 and this is
where many of the stage 2 discoveries will happen.

## Error handling

Same strategy as `naos-linux`: `anyhow::Result` everywhere, errors propagate to
`main`, unexpected vCPU exits are bugs not conditions. See
`DESIGN-naos-linux.md` for the full treatment.

## Testing strategy

Same philosophy as `naos-linux`: the integration test _is_ the unit test.
`cargo run -p naos-macos -- --kernel testdata/aarch64/Image --mem 256` produces
kernel boot messages and exits cleanly. Unit tests for the DTB builder (if we
end up writing one) and for register setup in `boot.rs`.

The test kernel for stage 2 is a separate artifact from stage 1 — an aarch64
Linux kernel built from source with a minimal config. Instructions will live in
`DEVELOPMENT.md` alongside the x86_64 instructions when stage 2 begins.

## Open questions

These are the questions that cannot be answered before stage 2 starts. They are
listed here so they are not forgotten and so the shape of the unknown is
visible.

- **Which Hypervisor.framework Rust binding.** `ahv`, `applevisor`, hand-rolled
  via `objc2`, or something that does not exist yet. Decide when stage 2 starts
  and pick whatever is actively maintained and fits the API shape we want.
- **How to build a minimal device tree blob.** Prebuilt artifact, runtime
  construction via `vm-fdt`, or build-time compilation from a DTS source file.
  Dominant question for stage 2 effort.
- **UART choice.** 16550 (reuse `vm-superio`) or PL011 (more idiomatic, needs
  emulation). Leaning toward 16550 for reuse.
- **Interrupt controller.** GIC v2, GIC v3, or none for the MVP (can we get away
  with no IRQ chip at all if we are not delivering interrupts)? The x86_64 MVP
  did not actually need the IRQ chip; the aarch64 MVP probably does not either,
  but this is worth verifying.
- **Whether `vm-memory`, `linux-loader`, and `vm-superio` work on macOS out of
  the box.** All three are pure Rust with no Linux-specific syscalls at their
  core, but "probably works" and "actually works" are different things. Verify
  during stage 2 and write minimal replacements if needed.
- **How to signal "kernel halted" cleanly.** On x86_64 it is `HLT`. On aarch64
  it is less obvious — probably a PSCI `SYSTEM_OFF` hypercall, maybe a specific
  WFI pattern. Worth reading the kernel's `arch/arm64/kernel/reboot.c` before
  writing the vCPU loop.
- **Where `TEXT_OFFSET` lives and how to compute it.** Documented in the
  kernel's boot protocol but subject to change between kernel versions. Worth
  reading the kernel source at stage-2 time to get the current answer.
- **How to handle the M5 Max specifically.** Hypervisor.framework on Apple
  Silicon is newer than the Intel flavor and has some quirks. The M5 Max is the
  author's target hardware and the only hardware stage 2 is tested on. If it
  works there, it ships.
