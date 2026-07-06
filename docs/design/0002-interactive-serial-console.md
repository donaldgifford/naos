---
id: DESIGN-0002
title: "Interactive serial console"
status: Draft
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# DESIGN 0002: Interactive serial console

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
  - [1. Serial input and interrupt injection](#1-serial-input-and-interrupt-injection)
  - [2. Registering the serial irqfd on GSI 4](#2-registering-the-serial-irqfd-on-gsi-4)
  - [3. Raw terminal mode](#3-raw-terminal-mode)
  - [4. initramfs load and zero-page wiring](#4-initramfs-load-and-zero-page-wiring)
  - [5. Guest kernel config and the initramfs artifact](#5-guest-kernel-config-and-the-initramfs-artifact)
  - [6. Wiring into the event-loop substrate](#6-wiring-into-the-event-loop-substrate)
  - [7. How this exercises the event loop](#7-how-this-exercises-the-event-loop)
- [API / Interface Changes](#api--interface-changes)
- [Data Model](#data-model)
- [Testing Strategy](#testing-strategy)
- [Migration / Rollout Plan](#migration--rollout-plan)
- [Open Questions](#open-questions)
  - [1. initramfs placement in guest RAM](#1-initramfs-placement-in-guest-ram)
  - [2. cmdline handling when an initramfs is present](#2-cmdline-handling-when-an-initramfs-is-present)
  - [3. Host escape hatch with ISIG off](#3-host-escape-hatch-with-isig-off)
  - [4. busybox provenance](#4-busybox-provenance)
- [References](#references)
<!--toc:end-->

## Overview

This design turns the boot-to-panic MVP into a VM you actually log into. On top
of the event-loop substrate from [[0001-event-loop-and-concurrency-model]] it
adds serial *input* (host stdin routed into the emulated UART's receive path,
with the receive interrupt delivered to the guest) and a busybox **initramfs**
rootfs so the kernel has a userspace to hand control to. It is also the first
real integration and functional test of that substrate: the deliverable is a
shell prompt on your terminal — run `ls` and `uname -a`, then `poweroff` for a
clean exit 0.

## Goals and Non-Goals

### Goals

- Deliver an **interactive serial console**: after `just run --initramfs ...`
  the terminal shows a shell prompt; keystrokes reach the guest; commands run;
  `poweroff` exits cleanly with status 0.
- Wire **stdin to UART RX** via `Serial::enqueue_raw_bytes`, and make the
  currently-dead `EventFdTrigger` **live** by registering it as a KVM **irqfd**
  on the serial GSI, so vm-superio pulsing the trigger injects IRQ 4.
- Put the host terminal in **raw mode** while the VM runs and restore it on exit
  (success, error, or panic), but only when stdin is a TTY.
- Load a small **initramfs** into guest RAM and point the kernel at it through
  the `boot_params` ramdisk fields, per
  [[0005-root-filesystem-initramfs-then-virtio-blk]].
- Add a `--initramfs <path>` CLI flag; **preserve the boot-to-panic path** when
  no initramfs is supplied.
- Serve as the **functional / acceptance test** that proves the event-loop
  substrate end-to-end: typing reaches the guest, and `poweroff` drives the
  shutdown protocol to exit 0.

### Non-Goals

- **The event loop itself.** The vCPU-thread + epoll-I/O-thread model, the
  `exit_evt` shutdown protocol, the vCPU wakeup mechanism, and the irqfd delivery
  substrate are all owned by [[0001-event-loop-and-concurrency-model]]. This
  design *consumes* that substrate and does not re-explain it.
- **virtio of any kind** (block, net, the MMIO bus). That is
  [[0003-virtio-mmio-device-model]] and [[0004-block-storage-via-virtio-blk]].
- **Networking / SSH.** Interactive access here is serial-only, exactly as
  [[0002-microvm-first-incremental-milestone-ladder]] scopes it; SSH is
  [[0005-guest-networking-and-ssh]].
- **Persistence.** The initramfs is a RAM rootfs; nothing survives poweroff.
- **SMP, bzImage, ACPI or MPTable, snapshot, jailer, control API.** Unchanged
  from the MVP non-goals in `DESIGN-naos-linux.md`.
- A **second emulated device**. The UART remains the only device; it just gains
  an input path and a live interrupt.

## Background

Today the UART is output-only. `serial::create` builds a
`Serial<EventFdTrigger, NoEvents, Stdout>` and the vCPU loop forwards guest THR
writes to stdout, but nothing ever reads stdin and no serial interrupt is ever
delivered. `serial.rs` says so plainly: the `EventFdTrigger` "exists only to
satisfy the type bound," because "we never deliver serial interrupts to the
guest." That is the gap this design closes.

Making the UART bidirectional needs a way to multiplex vCPU execution against
host-fd readiness: the moment host stdin can become ready *while the vCPU is
blocked inside `KVM_RUN`*, a single blocking loop can no longer express typing
into the guest. [[0001-event-loop-and-concurrency-model]] already solved that —
vCPU on its own thread, host I/O sources on an epoll loop, a shared `exit_evt`
for shutdown, and interrupts delivered through KVM irqfd. `WALK-linux.md` §13
flagged this as the pivot: "once you have an event loop, every future device
becomes cheap." This design is the first device to spend that substrate, and it
spends the smallest possible amount — **one** input source and **one**
interrupt — precisely so the substrate is exercised in isolation from any virtio
work.

The guest also needs somewhere to go after boot. Rather than the MVP's "no init
gives panic," [[0005-root-filesystem-initramfs-then-virtio-blk]] chose an
initramfs first: a cpio archive the VMM loads into guest RAM (like the kernel
itself) that the kernel unpacks as its rootfs, with zero device machinery.

## Detailed Design

### 1. Serial input and interrupt injection

This is the path that makes `EventFdTrigger` live. Stdin readiness is detected
by the epoll loop from [[0001-event-loop-and-concurrency-model]]; everything
below is the serial-specific wiring that hangs off that readiness event.

```text
 host stdin (fd 0, raw mode)
   |  epoll: readable   (polled by the event loop, see 0001)
   v
 read(2) N bytes ---> Serial::enqueue_raw_bytes(&buf)   (under the Mutex)
                        |  appends to the 16550 RX FIFO
                        |  if IER "received-data-available" is enabled,
                        v  pulses the Trigger:
                     EventFdTrigger::trigger() -> write(eventfd, 1)
                        |
                        v  eventfd registered as KVM irqfd on GSI 4
                     KVM injects IRQ 4 into the guest
                        |
                        v  guest 8250 ISR reads RBR (port 0x3F8, offset 0)
                     VcpuExit::IoIn 0x3F8 -> serial.read(0) drains a FIFO byte
```

- **RX enqueue.** When the event loop reports stdin readable, we read the
  available bytes and call vm-superio's `Serial::enqueue_raw_bytes`, which
  appends to the receive FIFO and, *when the guest has enabled the receive
  interrupt in IER*, pulses the configured `Trigger`.
- **Trigger to interrupt.** `EventFdTrigger::trigger` already writes `1` to its
  inner `EventFd` (see `serial.rs`). What changes is that the `EventFd` is now
  registered as an irqfd, so that write becomes a guest interrupt instead of a
  no-op nobody reads.
- **Drain.** The guest ISR reads the Receive Buffer Register — an `IoIn` at
  offset 0 in the COM1 range, already dispatched to `serial::handle_read`, which
  returns `serial.read(0)`, the next FIFO byte.
- **TX is unchanged.** The guest still writes THR (offset 0) and polls LSR
  (offset 5), handled by `serial::handle_write` / `serial::handle_read` exactly
  as in the MVP. This design only adds the RX half.

### 2. Registering the serial irqfd on GSI 4

The interrupt *delivery* substrate — registering an `EventFd` as an irqfd so a
host thread can inject without a userspace round-trip — belongs to
[[0001-event-loop-and-concurrency-model]]. What is specific to the serial device
is *which* fd and *which* GSI.

- During init we register the trigger's inner `EventFd` with
  `VmFd::register_irqfd(&event_fd, SERIAL_GSI)` where `SERIAL_GSI = 4` — COM1's
  legacy IRQ line (8250/16550, COM1 at ports 0x3F8 to 0x3FF, IRQ 4).
- The in-kernel IRQ chip created in `Vmm::new` (`create_irq_chip`) already routes
  GSI 4, so KVM injects the interrupt directly. This is exactly why the MVP
  created the IRQ chip "even though nothing raises interrupts yet"
  (`DESIGN-naos-linux.md`, piece 1).
- The `EventFd` must be `try_clone`d so both the `Serial` (which pulses it) and
  the `VmFd` (which owns the irqfd registration) hold a handle to the same
  underlying `eventfd(2)`; `EventFdTrigger` derefs to the inner `EventFd`, which
  already exposes `try_clone`.

### 3. Raw terminal mode

For keystrokes to pass through unbuffered and unechoed — the guest's line
discipline does its own echo and editing — the host TTY must be in raw mode.

- Use `libc::tcgetattr` to snapshot stdin's `termios`, `cfmakeraw` (or an
  explicit clear of `ICANON`, `ECHO`, `ISIG`, `IEXTEN`, `ICRNL`) to build the raw
  attributes, and `tcsetattr(TCSANOW)` to apply them. `libc` is already a
  dependency.
- A `RawTermGuard` holds the saved `termios` and restores it in `Drop`, so the
  terminal is always returned to cooked mode — on the success path, on an error
  return, and on a panic unwind.
- Apply raw mode only when stdin is a TTY (`libc::isatty`). When stdin is a pipe
  or file (tests, CI) skip it and still feed bytes through the RX path, so the
  acceptance test can script input without a pty.
- With `ISIG` cleared, Ctrl-C reaches the guest instead of killing naos; a host
  escape hatch for a wedged guest is an Open Question.

### 4. initramfs load and zero-page wiring

The initramfs is loaded like the kernel: copied into guest RAM by the VMM, then
advertised to the kernel through `boot_params`.

- **Placement.** Load the already-gzip-compressed cpio image high in guest RAM,
  page-aligned, top-down so it cannot overlap the kernel image (which the ELF
  loader places around 16 MiB). A small 256 to 512 MiB guest keeps the whole
  image below 4 GiB. This extends the memory map in `WALK-linux.md` §2.
- **boot_params fields** (Linux/x86 boot protocol,
  `Documentation/arch/x86/boot.rst`), written in `boot::write_boot_params`
  alongside the existing e820 and cmdline writes:
  - `hdr.ramdisk_image` — low 32 bits of the load physical address.
  - `hdr.ramdisk_size` — low 32 bits of the image size in bytes.
  - `ext_ramdisk_image` and `ext_ramdisk_size` — high 32 bits of each (boot
    protocol 2.12 and later). Zero for our sub-4-GiB placement, but wired so the
    field contract is complete and future large-RAM guests just work.
- **cmdline.** The kernel runs `/init` from the unpacked initramfs
  automatically; `rdinit=/init` makes it explicit. For interactive use the
  `panic=1` reboot-on-panic flag is dropped so a mistake does not reboot-loop the
  console, while `reboot=k` is kept so the `poweroff` reset lands on the clean
  exit path.
- **No-initramfs path.** When `--initramfs` is absent, none of the ramdisk
  fields are set (they stay zero), the default cmdline is unchanged, and the
  kernel panics on "no working init" exactly as the MVP does — the existing
  success signal and its tests stay intact.

### 5. Guest kernel config and the initramfs artifact

The `tinyconfig`-derived kernel from `scripts/build-test-kernel-x86_64.sh`
(currently `64BIT`, `PRINTK`, `EARLY_PRINTK`, `TTY`, `SERIAL_8250`,
`SERIAL_8250_CONSOLE`, `BINFMT_ELF`) gains:

| Config symbol | Why |
| ------------- | --- |
| `CONFIG_BLK_DEV_INITRD` | Enables initrd / initramfs unpacking (the whole feature). |
| `CONFIG_RD_GZIP` | Decompress the `.cpio.gz` in-kernel. |
| `CONFIG_DEVTMPFS` | A populated `/dev` (busybox needs `/dev/console`). |
| `CONFIG_DEVTMPFS_MOUNT` | Auto-mount devtmpfs at boot so `/dev` exists for init. |
| `CONFIG_PROC_FS`, `CONFIG_SYSFS` | Expected by a usable shell (`uname -a`, `ps`). |

The **initramfs artifact** is a static busybox packed into a cpio: an `/init`
(a tiny shell script that mounts `proc` and `sysfs` and execs `sh`), a
`/bin/busybox` with the usual applet symlinks, and a `/dev/console` node as a
fallback. It is built into `testdata/initramfs.cpio.gz`. The build story grows to
match `build-test-kernel-x86_64.sh`: a sibling `scripts/build-initramfs.sh` and a
`just initramfs` recipe, with the kernel script's `./scripts/config` block
extended with the symbols above.

### 6. Wiring into the event-loop substrate

The substrate from [[0001-event-loop-and-concurrency-model]] already puts the
`VcpuFd` on its own thread, shares the serial device as
`Arc<Mutex<Serial<...>>>`, and runs an epoll loop with an `exit_evt`. This
design plugs three things into it and touches init order minimally:

1. After **kernel load**, if `--initramfs` is set, load the image and remember
   its `(addr, size)`.
2. **boot_params** now also writes the ramdisk fields from that `(addr, size)`.
3. After **serial create**, register the serial `EventFd` as an irqfd on GSI 4
   (section 2), and register **stdin** as a subscriber on the epoll loop whose
   handler runs the RX path (section 1).
4. Set the host terminal raw via `RawTermGuard` (section 3) before the vCPU
   thread is spawned, so early guest output is already unbuffered.

No new threads, no new shutdown logic, and no change to the exit-reason
dispatch: `poweroff` still travels the guest `Hlt` / `Shutdown` / reset-request
path the MVP defined and the substrate wired to `exit_evt`.

### 7. How this exercises the event loop

This design is the substrate's first functional test. It touches all three of
the substrate's moving parts at once, which is why it is the right acceptance
gate for [[0001-event-loop-and-concurrency-model]]:

- **The epoll I/O thread** proves out by carrying real stdin readiness into
  `enqueue_raw_bytes` — the first non-`exit_evt` subscriber.
- **irqfd delivery** proves out because a host-side `EventFd` write (the RX
  trigger) becomes a guest-visible IRQ 4 with no userspace round-trip.
- **The shutdown protocol** proves out because the guest `poweroff` reaches the
  reset path, signals `exit_evt`, the vCPU thread is joined, and the process
  returns 0.

If any one of those three were broken, this design's acceptance test would fail
in an observable way (no echo, no interrupt drain, or a hung join), which is what
makes it a genuine end-to-end check rather than a smoke test.

## API / Interface Changes

- **New CLI flag** on `main::Args`:

  ```text
  --initramfs <PATH>   Optional path to a cpio.gz initramfs. Omitted -> boot-to-panic.
  ```

  Modeled as `initramfs: Option<PathBuf>`.
- **`--cmdline` default** is unchanged (still
  `console=ttyS0 reboot=k panic=1 pci=off`), preserving the MVP boot-to-panic
  run. Interactive users pass a cmdline without `panic=1`, or we derive one when
  an initramfs is present — see Open Questions.
- **`Vmm::new`** gains an `initramfs: Option<&Path>` parameter.
- **Internal module surface:**
  - `serial`: a helper to register the interrupt `EventFd` as an irqfd, and an
    RX-enqueue entry point wrapping `Serial::enqueue_raw_bytes`.
  - A new terminal raw-mode guard (`RawTermGuard`) alongside `serial` or in a
    small `term` module.
  - The event-loop and vCPU-thread plumbing comes from
    [[0001-event-loop-and-concurrency-model]]; this design only registers the
    stdin subscriber and the serial irqfd against it.
- No change to output, exit codes, or the existing exit-reason handling.

## Data Model

- **Guest physical memory map** gains one region (extends `WALK-linux.md` §2):

  ```text
  ... kernel (vmlinux ELF, ~16 MiB) ...
  <gap>
  initramfs cpio.gz  <- high in RAM, page-aligned, top-down (addr, size)
  top of guest RAM
  ```

- **boot_params additions** (in `boot::write_boot_params`): `hdr.ramdisk_image`,
  `hdr.ramdisk_size`, `ext_ramdisk_image`, `ext_ramdisk_size`. All zero when no
  initramfs.
- **Shared runtime state:** the serial device is already
  `Arc<Mutex<Serial<EventFdTrigger, NoEvents, Stdout>>>` per
  [[0001-event-loop-and-concurrency-model]]; this design adds a cloned serial
  interrupt `EventFd` held by the irqfd registration.
- **New constants:** `SERIAL_GSI: u32 = 4` (COM1 legacy IRQ) and an initramfs
  load-alignment constant. The saved `termios` lives in the `RawTermGuard`.

## Testing Strategy

The existing KVM-gated pattern (skip cleanly when `/dev/kvm` is inaccessible)
carries over. Following `DESIGN-naos-linux.md`, unit tests cover the constants
most likely to be wrong, and one end-to-end check exercises the whole thing.

- **Unit, `boot.rs`.** Extend the boot_params tests: with an initramfs,
  `ramdisk_image` and `ramdisk_size` (and `ext_*` for a synthetic address above
  4 GiB) hold the expected low/high split; with no initramfs the ramdisk fields
  are all zero — a regression guard for the boot-to-panic path.
- **Unit, `serial.rs`.** `enqueue_raw_bytes` followed by `handle_read` at offset
  0 returns the bytes in order; enqueuing with the RX interrupt enabled pulses
  the trigger, observable on the inner `EventFd`.
- **Unit, irqfd and termios.** irqfd registration on GSI 4 succeeds against a
  real `VmFd` (KVM-gated); `RawTermGuard` restores the original `termios` on drop
  (against a pty).
- **Integration, the success criterion.** Drive `naos-linux --kernel
  testdata/vmlinux --initramfs testdata/initramfs.cpio.gz` with a scripted stdin
  (expect-style): wait for the shell prompt, send `ls` and `uname -a`, assert
  plausible output, send `poweroff`, assert **exit 0**. This is the functional
  test that gates [[0001-event-loop-and-concurrency-model]].
- **Regression.** The MVP e2e (no `--initramfs`, boots to panic, exits 0) must
  still pass unchanged.

**Success criterion (stated):** `just run --initramfs ...` yields a shell prompt
on the terminal; `ls` and `uname -a` run; `poweroff` gives a clean exit 0.

## Migration / Rollout Plan

This design lands *after* [[0001-event-loop-and-concurrency-model]] — the
behavior-preserving substrate (vCPU thread, epoll loop, `exit_evt`) is a
prerequisite and ships first, with the VM still booting to panic and exiting 0.
On top of that, two landable steps, each keeping the tree green and the
no-initramfs boot-to-panic path intact:

1. **Serial input, interrupt, and raw mode.** Register the serial irqfd on
   GSI 4, register the stdin subscriber that calls `enqueue_raw_bytes`, and add
   the `RawTermGuard`. Typing now reaches the guest; without a rootfs it still
   panics, but the RX path and IRQ injection are exercised end-to-end.
2. **initramfs, kernel config, and artifact.** Add `--initramfs`, the boot_params
   ramdisk wiring, the extended kernel config, and the `build-initramfs.sh` and
   `just initramfs` artifact. Now `just run --initramfs ...` reaches a shell —
   the design is complete.

Build-artifact rollout: land the initramfs builder and the kernel-config deltas
alongside step 2, document them next to the existing kernel-build instructions in
`DEVELOPMENT.md`, and keep `testdata/initramfs.cpio.gz` a generated (not
committed) artifact, mirroring `testdata/vmlinux`.

## Open Questions

Each item is a decision to settle before this design moves from Draft to
Approved. Option **a** is the recommendation; **b** onward are alternatives;
**other** is a write-in. Record the choice on the **Decision** line.

### 1. initramfs placement in guest RAM

- **a (recommended).** Load top-down from the top of guest RAM, page-aligned,
  with a guard that rejects an image overlapping the kernel or larger than RAM.
- **b.** A fixed low load address (Firecracker-style) — simpler, less flexible
  for large images.
- **other.** *(write-in)*

**Decision:** a — top-down, page-aligned, with an overlap/oversize guard.

### 2. cmdline handling when an initramfs is present

- **a (recommended).** Derive an interactive default when `--initramfs` is set
  (append `rdinit=/init`, drop `panic=1`) unless the user overrides `--cmdline`.
- **b.** Keep the `--cmdline` default fixed and require the user to pass an
  interactive cmdline themselves.
- **other.** *(write-in)*

**Decision:** a — derive an interactive default unless `--cmdline` is given.

### 3. Host escape hatch with ISIG off

- **a (recommended).** A recognized stdin escape sequence (QEMU-style
  Ctrl-a x) that force-exits naos.
- **b.** No in-band escape — the operator kills the process from another
  terminal.
- **other.** *(write-in)*

**Decision:** a — QEMU-style `Ctrl-a x` escape.

### 4. busybox provenance

- **a (recommended).** Build a static busybox from source in the artifact script
  (consistent with building the test kernel from source), and rely on devtmpfs
  auto-mount for `/dev` with a cpio-baked `/dev/console` fallback.
- **b.** Vendor a known-good prebuilt static busybox binary — faster, less build
  machinery.
- **other.** *(write-in)*

**Decision:** a — build static busybox from source.

## References

- Substrate: [[0001-event-loop-and-concurrency-model]] (vCPU thread, epoll I/O
  loop, `exit_evt` shutdown, irqfd delivery — consumed here, not re-specified).
- ADRs: [[0002-microvm-first-incremental-milestone-ladder]],
  [[0003-event-driven-epoll-concurrency-model]],
  [[0005-root-filesystem-initramfs-then-virtio-blk]].
- Sibling designs: [[0003-virtio-mmio-device-model]],
  [[0004-block-storage-via-virtio-blk]], [[0005-guest-networking-and-ssh]].
- `DESIGN-naos-linux.md`, `WALK-linux.md` (§2 memory map, §7 serial, §13
  "What's next").
- Linux/x86 boot protocol — `Documentation/arch/x86/boot.rst`: `ramdisk_image`,
  `ramdisk_size`, `ext_ramdisk_image`, `ext_ramdisk_size`, and the `boot_params`
  layout.
- Kernel config symbols: `CONFIG_BLK_DEV_INITRD`, `CONFIG_RD_GZIP`,
  `CONFIG_DEVTMPFS`, `CONFIG_DEVTMPFS_MOUNT`.
- 8250/16550 UART: TI PC16550D datasheet (RX FIFO, IER received-data-available,
  RBR and LSR registers); COM1 at ports 0x3F8 to 0x3FF, IRQ 4.
- KVM API — `Documentation/virt/kvm/api.html`: `KVM_IRQFD`, in-kernel IRQ chip.
- rust-vmm crates: `vm-superio` (`Serial::enqueue_raw_bytes`, `Trigger`),
  `kvm-ioctls` (`VmFd::register_irqfd`), `vmm-sys-util` (`EventFd`),
  `libc` (termios: `tcgetattr`, `cfmakeraw`, `tcsetattr`, `isatty`).
