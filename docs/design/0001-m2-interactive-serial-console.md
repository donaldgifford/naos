---
id: DESIGN-0001
title: "M2 — interactive serial console"
status: Draft
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# DESIGN 0001: M2 — interactive serial console

**Status:** Draft
**Author:** Donald Gifford
**Date:** 2026-07-05

<!--toc:start-->
- [Overview](#overview)
- [Goals and Non-Goals](#goals-and-non-goals)
  - [Goals](#goals)
  - [Non-Goals](#non-goals)
- [Background](#background)
- [Detailed Design](#detailed-design)
  - [1. Concurrency and threading model](#1-concurrency-and-threading-model)
  - [2. Clean shutdown](#2-clean-shutdown)
  - [3. Serial input and interrupt injection](#3-serial-input-and-interrupt-injection)
  - [4. Raw terminal mode](#4-raw-terminal-mode)
  - [5. initramfs load and zero-page wiring](#5-initramfs-load-and-zero-page-wiring)
  - [6. Guest kernel config and the initramfs artifact](#6-guest-kernel-config-and-the-initramfs-artifact)
  - [7. Init order in Vmm::new](#7-init-order-in-vmmnew)
- [API / Interface Changes](#api--interface-changes)
- [Data Model](#data-model)
- [Testing Strategy](#testing-strategy)
- [Migration / Rollout Plan](#migration--rollout-plan)
- [Open Questions](#open-questions)
  - [1. Event-loop implementation](#1-event-loop-implementation)
  - [2. vCPU-thread wakeup mechanism](#2-vcpu-thread-wakeup-mechanism)
  - [3. initramfs placement in guest RAM](#3-initramfs-placement-in-guest-ram)
  - [4. cmdline handling when an initramfs is present](#4-cmdline-handling-when-an-initramfs-is-present)
  - [5. Host escape hatch with ISIG off](#5-host-escape-hatch-with-isig-off)
  - [6. Serial device sharing granularity](#6-serial-device-sharing-granularity)
  - [7. busybox provenance](#7-busybox-provenance)
- [References](#references)
<!--toc:end-->

## Overview

M2 turns the boot-to-panic MVP into a VM you actually log into. It adds three
things on top of the existing `naos-linux` core: an event-driven concurrency
model (a vCPU thread plus an epoll I/O thread), serial *input* (host stdin routed
into the emulated UART's receive path, with the receive interrupt delivered to
the guest), and a busybox **initramfs** rootfs so there is a userspace for the
kernel to hand control to. The deliverable is a shell prompt on your terminal:
run `ls` and `uname -a`, then `poweroff` for a clean exit 0.

## Goals and Non-Goals

### Goals

- Deliver an **interactive serial console**: after `just run` the terminal shows
  a shell prompt; keystrokes reach the guest; commands run; `poweroff` exits
  cleanly with status 0.
- Move from the single blocking `vcpu::run` loop to a **vCPU thread + epoll I/O
  thread** model, per [[0003-event-driven-epoll-concurrency-model]], with a
  defined coordination and shutdown protocol.
- Wire **stdin → UART RX** and make the currently-dead `EventFdTrigger` live by
  registering it as a KVM **irqfd** on the serial GSI, so vm-superio pulsing the
  trigger injects IRQ 4 into the guest.
- Load a small **initramfs** into guest RAM and point the kernel at it via the
  `boot_params` ramdisk fields, per [[0005-root-filesystem-initramfs-then-virtio-blk]].
- Add a `--initramfs <path>` CLI flag; **preserve the boot-to-panic path** when
  no initramfs is supplied.
- Put the host terminal in raw mode while the VM runs and restore it on exit
  (including on error/panic).

### Non-Goals

- **virtio of any kind** (block, net, the MMIO bus). That is M3
  ([[0002-m3-block-storage-via-virtio-blk]]) and M4 ([[0003-m4-guest-networking-and-ssh]]).
- **Networking / SSH.** Interactive access at M2 is serial-only, exactly as
  [[0002-microvm-first-incremental-milestone-ladder]] scopes it.
- **Persistence.** The initramfs is a RAM rootfs; nothing survives poweroff.
- **SMP, bzImage, ACPI/MPTable, snapshot, jailer, control API.** Unchanged from
  the MVP non-goals in `DESIGN-naos-linux.md`.
- A second emulated device. The UART remains the only device; it just gains an
  input path and a live interrupt.

## Background

Today the VMM is a single thread. `vmm::Vmm::run` calls `vcpu::run`, which loops
on `VcpuFd::run` (blocking `KVM_RUN`) and dispatches `VcpuExit::IoOut` /
`IoIn` in the COM1 range to `serial::handle_write` / `handle_read`, with `Hlt`,
`Shutdown`, and the recognized platform-reset port writes (`is_reset_request`)
all breaking the loop for a clean exit. The UART is built by `serial::create` as
`Serial<EventFdTrigger, NoEvents, Stdout>` — output only. `serial.rs` says so
plainly: the `EventFdTrigger` "exists only to satisfy the type bound," because we
never deliver a serial interrupt and never read stdin.

That single-loop model cannot express typing into the guest. The moment there is
a *second* I/O source (host stdin) that becomes ready asynchronously while the
vCPU is blocked inside `KVM_RUN`, we must multiplex vCPU execution against
host-fd readiness. [[0003-event-driven-epoll-concurrency-model]] decided the
shape: vCPU on its own thread (still blocking in `KVM_RUN`), host I/O sources on
an epoll loop on another thread, interrupts delivered via KVM irqfd. This is the
pivotal change `WALK-linux.md` §13 flagged — "once you have an event loop, every
future device becomes cheap" — and M2 is where it lands, with the smallest
possible payload (one input source, one interrupt) so the threading change is
de-risked in isolation from any virtio work.

The guest also needs somewhere to go after boot. Rather than the MVP's "no init
→ panic," [[0005-root-filesystem-initramfs-then-virtio-blk]] chose an initramfs
first: a cpio archive the VMM loads into guest RAM (like the kernel itself) that
the kernel unpacks as its rootfs, with zero device machinery.

## Detailed Design

### 1. Concurrency and threading model

Two threads share three things: guest memory, the serial device, and a shutdown
signal.

```text
 main thread  ──────────────────────────────────────────────
   Vmm::new()  (unchanged init order, + initramfs load)
   register serial EventFd as irqfd(GSI 4)
   set host terminal raw  (RAII guard)
   spawn vCPU thread ──────────────┐
   run epoll I/O loop  (this thread)│
        │                          │
   ┌────┴─────────────┐            │
   │ epoll_wait       │            ▼
   │  • stdin readable │      vCPU thread
   │    → lock serial, │      loop { KVM_RUN
   │      enqueue RX,  │        IoOut/IoIn 0x3F8.. → lock serial
   │      (trigger →   │        Hlt/Shutdown/reset → signal exit, break
   │       irqfd →     │        other → error, signal exit, break
   │       IRQ 4)      │      }
   │  • exit_evt       │            │
   │    → break        │◄───────────┘ writes exit_evt on break
   └──────────────────┘
   join vCPU thread; restore terminal; return status
```

- **Guest memory** becomes `Arc<GuestMemoryMmap>`. KVM already holds the mapping
  independently (`set_user_memory_region`), but both threads — and every future
  device backend — need a handle, and `Arc` is the idiomatic rust-vmm sharing.
- **The serial device** becomes `Arc<Mutex<Serial<EventFdTrigger, NoEvents,
  Stdout>>>`. The vCPU thread locks it to service PIO exits; the I/O thread locks
  it to enqueue received bytes. Contention is negligible at human/serial rates —
  the lock is held only for a register access or a short FIFO push.
- **Ownership / lifetime.** KVM requires `VmFd` to outlive `VcpuFd`, and guest
  memory to outlive `VmFd`. The `Vmm` struct (owning `Kvm`, `VmFd`, and the `Arc`
  memory) stays on the main thread; the `VcpuFd` is *moved* into the spawned vCPU
  thread, which is **joined before** `Vmm` drops. So the existing drop-order
  invariant is preserved across the thread boundary.
- **Event loop implementation.** rust-vmm's `event-manager` or a thin `epoll`
  wrapper over `vmm-sys-util`'s `EventFd`/epoll helpers. Per
  [[0003-event-driven-epoll-concurrency-model]] this is left to implementation;
  see Open Questions.

### 2. Clean shutdown

Shutdown must terminate *both* threads no matter which one initiates it, and it
must still fire on the existing exit signals (guest `Hlt`, `Shutdown`, or a
recognized reset-request port write — the `poweroff` path).

- A shared **`exit_evt: EventFd`** is registered with the epoll loop.
- **Guest-initiated (the normal case).** The vCPU thread's loop breaks on
  `Hlt` / `Shutdown` / `is_reset_request` exactly as today, then writes
  `exit_evt`. The I/O thread wakes on `exit_evt`, leaves `epoll_wait`, and the
  main thread joins the vCPU thread. `poweroff` in the guest lands here (busybox
  `poweroff` → kernel `machine_power_off`, which reaches our reset/`Hlt` path),
  yielding exit 0.
- **Host-initiated (error / stdin EOF).** If the I/O thread hits a fatal error
  it must stop the vCPU, which may be blocked in `KVM_RUN`. We set
  `VcpuFd::set_kvm_immediate_exit(1)` and send the vCPU thread a signal (a no-op
  `SIGUSR1` handler) so the in-flight `KVM_RUN` returns promptly with
  `EINTR`/immediate-exit; the vCPU loop observes the shared "stop" flag and
  breaks.
- The terminal is restored by the raw-mode RAII guard as it drops on the main
  thread, so it is restored on the success path, on an error return, and on a
  panic unwind.

### 3. Serial input and interrupt injection

This is the path that makes `EventFdTrigger` live.

```text
 host stdin (fd 0, raw mode)
   │  epoll: readable
   ▼
 read(2) N bytes ──► Serial::enqueue_raw_bytes(&buf)   (under the Mutex)
                        │  pushes into the 16550 RX FIFO
                        │  if IER "received-data-available" is enabled,
                        ▼  pulses the Trigger:
                     EventFdTrigger::trigger() → write(eventfd, 1)
                        │
                        ▼  eventfd registered as KVM irqfd on GSI 4
                     KVM injects IRQ 4 into the guest
                        │
                        ▼  guest 8250 ISR reads RBR (port 0x3F8, offset 0)
                     VcpuExit::IoIn 0x3F8 → serial.read(0) drains a FIFO byte
```

- **RX enqueue.** On stdin readable, read available bytes and call vm-superio's
  `Serial::enqueue_raw_bytes`, which appends to the receive FIFO and, when the
  guest has enabled the receive interrupt, pulses the configured `Trigger`.
- **irqfd.** During init we register the trigger's inner `EventFd` as an irqfd:
  `VmFd::register_irqfd(&event_fd, SERIAL_GSI)` with `SERIAL_GSI = 4` (COM1's
  legacy IRQ). The in-kernel IRQ chip already created in `Vmm::new`
  (`create_irq_chip`) routes GSI 4, so KVM injects the interrupt with no
  userspace round-trip. This is precisely why the MVP created the IRQ chip
  "even though nothing raises interrupts yet."
- **Drain.** The guest ISR reads the Receive Buffer Register; that is an
  `IoIn` at offset 0 in the COM1 range, already dispatched to
  `serial::handle_read`, which returns `serial.read(0)` — the next FIFO byte.
- No change to the TX path: the guest still writes THR (offset 0) and polls LSR
  (offset 5), handled by `serial::handle_write` / `handle_read` today.

### 4. Raw terminal mode

For keystrokes to pass through unbuffered and unechoed (the guest's line
discipline does echo and editing), the host TTY must be in raw mode.

- Use `libc::tcgetattr` to snapshot stdin's `termios`, `cfmakeraw` (or an
  explicit clear of `ICANON`/`ECHO`/`ISIG`/`IEXTEN`/`ICRNL`) to build the raw
  attributes, and `tcsetattr(TCSANOW)` to apply them. `libc` is already a
  dependency.
- A `RawTermGuard` holds the saved `termios` and restores it in `Drop`, so the
  terminal is always returned to cooked mode.
- Only apply raw mode when stdin is a TTY (`isatty`); when stdin is a pipe or
  file (tests, CI) skip it and still feed bytes through the RX path.
- With `ISIG` cleared, Ctrl-C reaches the guest instead of killing naos; a host
  escape hatch for a wedged guest is an Open Question.

### 5. initramfs load and zero-page wiring

The initramfs is loaded like the kernel: copied into guest RAM by the VMM, then
advertised to the kernel through `boot_params`.

- **Placement.** Load the (already-gzip-compressed) cpio image high in guest
  RAM, page-aligned, top-down so it cannot overlap the kernel image (which the
  ELF loader places around 16 MiB). A small 256–512 MiB guest keeps the whole
  image below 4 GiB.
- **boot_params fields** (Linux/x86 boot protocol,
  `Documentation/arch/x86/boot.rst`):
  - `hdr.ramdisk_image` — low 32 bits of the load physical address.
  - `hdr.ramdisk_size` — low 32 bits of the image size in bytes.
  - `ext_ramdisk_image` / `ext_ramdisk_size` — high 32 bits of each (boot
    protocol ≥ 2.12). Zero for our sub-4-GiB placement, but wired so the field
    contract is complete and future large-RAM guests just work.
- **cmdline.** The kernel runs `/init` from the unpacked initramfs
  automatically; `rdinit=/init` makes it explicit. For interactive use the
  `panic=1` reboot-on-panic flag is dropped so a mistake doesn't reboot-loop the
  console, while `reboot=k` is kept so the `poweroff` reset lands on our clean
  exit.
- **No-initramfs path.** When `--initramfs` is absent, none of the ramdisk
  fields are set, the default cmdline is unchanged, and the kernel panics on
  "no working init" exactly as the MVP does — the existing success signal and its
  tests stay intact.

### 6. Guest kernel config and the initramfs artifact

The `tinyconfig`-derived kernel from `scripts/build-test-kernel-x86_64.sh`
(currently `64BIT`, `PRINTK`, `EARLY_PRINTK`, `TTY`, `SERIAL_8250`,
`SERIAL_8250_CONSOLE`, `BINFMT_ELF`) gains:

| Config symbol            | Why                                                     |
| ------------------------ | ------------------------------------------------------- |
| `CONFIG_BLK_DEV_INITRD`  | Enables initrd/initramfs unpacking (the whole feature). |
| `CONFIG_RD_GZIP`         | Decompress the `.cpio.gz` in-kernel.                    |
| `CONFIG_DEVTMPFS`        | A populated `/dev` (busybox needs `/dev/console`).      |
| `CONFIG_DEVTMPFS_MOUNT`  | Auto-mount devtmpfs at boot so `/dev` exists for init.  |
| `CONFIG_PROC_FS`, `CONFIG_SYSFS` | Expected by a usable shell (`uname -a`, `ps`).  |

The **initramfs artifact** is a static busybox packed into a cpio: `/init`
(a tiny shell script or a symlink to busybox that mounts `proc`/`sysfs` and execs
`sh`), `/bin/busybox` with the usual applet symlinks, and a `/dev/console` node
as a fallback. It is built into `testdata/initramfs.cpio.gz`. The build story
grows to match `build-test-kernel-x86_64.sh`: a sibling
`scripts/build-initramfs.sh` and a `just initramfs` recipe, with the kernel
script's `./scripts/config` block extended with the symbols above.

### 7. Init order in `Vmm::new`

The current order (KVM → VM → TSS → IRQ chip → PIT → memory → kernel load →
boot_params → serial → vCPU → CPUID → configure) changes minimally:

1. After **kernel load**, if `--initramfs` is set, load the image and remember
   its `(addr, size)`.
2. **boot_params** now also writes the ramdisk fields from that `(addr, size)`.
3. After **serial create**, register the serial `EventFd` as an irqfd on GSI 4.
4. `Vmm::run` no longer calls `vcpu::run` directly: it moves the `VcpuFd` and the
   shared `Arc`s into a spawned vCPU thread and drives the epoll loop, then joins.

## API / Interface Changes

- **New CLI flag** on `main::Args`:

  ```text
  --initramfs <PATH>   Optional path to a cpio.gz initramfs. Omitted → boot-to-panic.
  ```

  Modeled as `initramfs: Option<PathBuf>`.
- **`--cmdline` default** is unchanged (still
  `console=ttyS0 reboot=k panic=1 pci=off`), preserving the MVP boot-to-panic
  run. Interactive users pass a cmdline without `panic=1` (or we derive one when
  an initramfs is present — see Open Questions).
- **`Vmm::new`** gains an `initramfs: Option<&Path>` parameter.
- **Internal module surface:**
  - `serial`: a helper to register the interrupt `EventFd` as an irqfd, and an
    RX-enqueue entry point wrapping `Serial::enqueue_raw_bytes`.
  - `vcpu::run` keeps its exit-dispatch semantics but is invoked on a dedicated
    thread and takes the shared `Arc<Mutex<Serial<...>>>` plus the stop signal.
  - A new terminal/raw-mode guard and a new event-loop module (`io` or
    `event_loop`).
- No change to output, exit codes, or the existing exit-reason handling.

## Data Model

- **Guest physical memory map** gains one region (extends the map in
  `WALK-linux.md` §2):

  ```text
  ... kernel (vmlinux ELF, ~16 MiB) ...
  <gap>
  initramfs cpio.gz  ← high in RAM, page-aligned, top-down (addr, size)
  top of guest RAM
  ```

- **boot_params additions** (in `boot::write_boot_params`, alongside the existing
  e820 + cmdline writes): `hdr.ramdisk_image`, `hdr.ramdisk_size`,
  `ext_ramdisk_image`, `ext_ramdisk_size`. All zero when no initramfs.
- **Shared runtime state:** `Arc<GuestMemoryMmap>`,
  `Arc<Mutex<Serial<EventFdTrigger, NoEvents, Stdout>>>`, an `exit_evt: EventFd`,
  and an atomic stop flag.
- **New constants:** `SERIAL_GSI: u32 = 4` (COM1 legacy IRQ), and an initramfs
  load-alignment constant. The saved `termios` lives in the `RawTermGuard`.

## Testing Strategy

The existing KVM-gated pattern (skip cleanly when `/dev/kvm` is inaccessible)
carries over. Following `DESIGN-naos-linux.md`, unit tests cover the constants
most likely to be wrong, and one end-to-end check exercises the whole thing.

- **Unit — `boot.rs`.** Extend the boot_params tests: with an initramfs,
  `ramdisk_image` / `ramdisk_size` (and `ext_*` for a synthetic >4 GiB address)
  hold the expected low/high split; with no initramfs the ramdisk fields are all
  zero (regression guard for the boot-to-panic path).
- **Unit — `serial.rs`.** `enqueue_raw_bytes` followed by `handle_read(offset 0)`
  returns the bytes in order; enqueuing with the RX interrupt enabled pulses the
  trigger (observable on the inner `EventFd`).
- **Unit — irqfd / termios.** irqfd registration on GSI 4 succeeds against a real
  `VmFd` (KVM-gated); `RawTermGuard` restores the original `termios` on drop
  (against a pty).
- **Integration — the success criterion.** Drive `naos-linux --kernel
  testdata/vmlinux --initramfs testdata/initramfs.cpio.gz` with a scripted stdin
  (expect-style): wait for the shell prompt, send `ls` and `uname -a`, assert
  plausible output, send `poweroff`, assert **exit 0**. This is the M2 acceptance
  test.
- **Regression.** The MVP e2e (no `--initramfs`, boots to panic, exits 0) must
  still pass unchanged.

**Success criterion (stated):** `just run` (with the initramfs) yields a shell
prompt on the terminal; `ls` and `uname -a` run; `poweroff` → clean exit 0.

## Migration / Rollout Plan

Three landable steps, each keeping the tree green and the no-initramfs
boot-to-panic path intact:

1. **Event loop, behavior-preserving.** Introduce the vCPU thread + epoll I/O
   thread and the `exit_evt` shutdown protocol with the serial device still
   output-only. The VM still boots to panic and exits 0 — no user-visible change,
   but the threading and shutdown are now proven in isolation.
2. **Serial input + interrupt + raw mode.** Register the serial irqfd (GSI 4),
   wire stdin readable → `enqueue_raw_bytes`, and add the `RawTermGuard`. Typing
   now reaches the guest; without a rootfs it still panics, but the input path
   and IRQ injection are exercised end-to-end.
3. **initramfs + kernel config + artifact.** Add `--initramfs`, the boot_params
   ramdisk wiring, the extended kernel config, and the `build-initramfs.sh` /
   `just initramfs` artifact. Now `just run` reaches a shell — M2 complete.

Build-artifact rollout: land the initramfs builder and the kernel-config deltas
alongside step 3, document them next to the existing kernel-build instructions in
`DEVELOPMENT.md`, and keep `testdata/initramfs.cpio.gz` a generated (not
committed) artifact, mirroring `testdata/vmlinux`.

## Open Questions

Each item is a decision to settle before this design moves from Draft to
Approved. Option **a** is the recommendation; **b** onward are alternatives;
**other** is a write-in. Record the choice on the **Decision** line.

### 1. Event-loop implementation

- **a (recommended).** A thin hand-rolled epoll wrapper over `vmm-sys-util` — at
  M2 the loop has one real subscriber (stdin) plus `exit_evt`, so the minimal
  thing wins; reconsider `event-manager` at M3 when devices multiply.
- **b.** Adopt `event-manager` now, taking a heavier dependency to avoid a
  rewrite when virtio adds subscribers.
- **other.** *(write-in)*

**Decision:** *pending*

### 2. vCPU-thread wakeup mechanism

- **a (recommended).** `set_kvm_immediate_exit(1)` plus a no-op `SIGUSR1` to the
  vCPU thread (the Firecracker approach); confirm it reliably breaks an in-flight
  `KVM_RUN` and settle where the handler is installed.
- **b.** No forced wakeup — rely only on the guest's own `Hlt`/reset to end the
  loop; simpler, but cannot stop a wedged guest.
- **other.** *(write-in)*

**Decision:** *pending*

### 3. initramfs placement in guest RAM

- **a (recommended).** Load top-down from the top of guest RAM, page-aligned,
  with a guard that rejects an image overlapping the kernel or larger than RAM.
- **b.** A fixed low load address (Firecracker-style); simpler, less flexible for
  large images.
- **other.** *(write-in)*

**Decision:** *pending*

### 4. cmdline handling when an initramfs is present

- **a (recommended).** Derive an interactive default when `--initramfs` is set
  (append `rdinit=/init`, drop `panic=1`) unless the user overrides `--cmdline`.
- **b.** Keep the `--cmdline` default fixed and require the user to pass an
  interactive cmdline themselves.
- **other.** *(write-in)*

**Decision:** *pending*

### 5. Host escape hatch with ISIG off

- **a (recommended).** A recognized stdin escape sequence (QEMU-style
  `Ctrl-a x`) that force-exits naos.
- **b.** No in-band escape — the operator kills the process from another
  terminal.
- **other.** *(write-in)*

**Decision:** *pending*

### 6. Serial device sharing granularity

- **a (recommended).** A single `Arc<Mutex<Serial>>`; contention is negligible at
  serial rates.
- **b.** Split TX (vCPU thread) from RX (I/O thread) locks if hold time ever
  matters.
- **other.** *(write-in)*

**Decision:** *pending*

### 7. busybox provenance

- **a (recommended).** Build a static busybox from source in the artifact script
  (consistent with building the test kernel from source), and rely on devtmpfs
  auto-mount for `/dev` with a cpio-baked `/dev/console` fallback.
- **b.** Vendor a known-good prebuilt static busybox binary — faster, less build
  machinery.
- **other.** *(write-in)*

**Decision:** *pending*

## References

- ADRs: [[0002-microvm-first-incremental-milestone-ladder]],
  [[0003-event-driven-epoll-concurrency-model]],
  [[0005-root-filesystem-initramfs-then-virtio-blk]].
- Sibling designs: [[0002-m3-block-storage-via-virtio-blk]],
  [[0003-m4-guest-networking-and-ssh]].
- `DESIGN-naos-linux.md`, `WALK-linux.md` (§2 memory map, §7 serial, §13
  "What's next").
- Linux/x86 boot protocol — `Documentation/arch/x86/boot.rst`: `ramdisk_image`,
  `ramdisk_size`, `ext_ramdisk_image`/`ext_ramdisk_size`, and `struct
  boot_params` layout.
- Kernel config symbols: `CONFIG_BLK_DEV_INITRD`, `CONFIG_RD_GZIP`,
  `CONFIG_DEVTMPFS`, `CONFIG_DEVTMPFS_MOUNT`.
- 8250/16550 UART: TI PC16550D datasheet (RX FIFO, IER receive-data-available,
  RBR/LSR registers); COM1 = ports 0x3F8–0x3FF, IRQ 4.
- KVM API — `Documentation/virt/kvm/api.html`: `KVM_IRQFD`,
  `KVM_SET_KVM_IMMEDIATE_EXIT`, in-kernel IRQ chip.
- rust-vmm crates: `event-manager`, `vm-superio` (`Serial::enqueue_raw_bytes`,
  `Trigger`), `kvm-ioctls` (`VmFd::register_irqfd`,
  `VcpuFd::set_kvm_immediate_exit`), `vmm-sys-util` (`EventFd`, epoll).
