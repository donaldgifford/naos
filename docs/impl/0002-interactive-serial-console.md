---
id: IMPL-0002
title: "Interactive serial console"
status: Draft
author: Donald Gifford
created: 2026-07-06
---
<!-- markdownlint-disable-file MD025 MD041 -->

# IMPL 0002: Interactive serial console

**Status:** Draft
**Author:** Donald Gifford
**Date:** 2026-07-06

<!--toc:start-->
- [Objective](#objective)
- [Scope](#scope)
  - [In Scope](#in-scope)
  - [Out of Scope](#out-of-scope)
- [Current State](#current-state)
- [Dependencies](#dependencies)
- [Implementation Phases](#implementation-phases)
  - [Phase 1: Raw terminal mode](#phase-1-raw-terminal-mode)
    - [Tasks](#tasks)
    - [Success Criteria](#success-criteria)
  - [Phase 2: Host stdin to UART receive](#phase-2-host-stdin-to-uart-receive)
    - [Tasks](#tasks-1)
    - [Success Criteria](#success-criteria-1)
  - [Phase 3: Interactive cmdline and initramfs load](#phase-3-interactive-cmdline-and-initramfs-load)
    - [Tasks](#tasks-2)
    - [Success Criteria](#success-criteria-2)
  - [Phase 4: Guest busybox rootfs](#phase-4-guest-busybox-rootfs)
    - [Tasks](#tasks-3)
    - [Success Criteria](#success-criteria-3)
  - [Phase 5: Tests for input and interactive boot](#phase-5-tests-for-input-and-interactive-boot)
    - [Tasks](#tasks-4)
    - [Success Criteria](#success-criteria-4)
- [Open Questions](#open-questions)
  - [1. Termios save and restore and panic safety](#1-termios-save-and-restore-and-panic-safety)
  - [2. Detecting the Ctrl-a x escape sequence](#2-detecting-the-ctrl-a-x-escape-sequence)
  - [3. Where the initramfs lands in the current memory map](#3-where-the-initramfs-lands-in-the-current-memory-map)
  - [4. Producing a statically linked busybox](#4-producing-a-statically-linked-busybox)
- [File Changes](#file-changes)
- [Testing Plan](#testing-plan)
- [References](#references)
<!--toc:end-->

## Objective

Make the emulated 16550 UART bidirectional so the guest becomes an interactive
serial console. Host stdin is routed into vm-superio's receive FIFO, the
receive interrupt is delivered to the guest via KVM `irqfd` on GSI 4, and the
host terminal runs in raw mode with a QEMU-style `Ctrl-a x` escape hatch. A
busybox initramfs gives the guest a userspace, so `just run --initramfs ...`
yields a shell prompt, runs commands, and exits 0 on `poweroff`.

**Implements:** [[0002-interactive-serial-console]]

## Scope

### In Scope

- Raw terminal mode for host stdin with a RAII guard that restores the terminal
  on the success path, on error, and on panic (only when stdin is a TTY).
- The stdin to UART receive path: read host stdin, `Serial::enqueue_raw_bytes`
  into the 16550 RX FIFO, and raise IRQ 4 via an `EventFd` registered as a KVM
  `irqfd`, making the currently-dead `EventFdTrigger` live.
- A `Ctrl-a x` byte-level escape state machine on the host RX path that requests
  a clean shutdown.
- A `--initramfs <PATH>` CLI flag, an interactive cmdline default derived when
  an initramfs is present, and `boot_params` ramdisk wiring.
- Top-down, page-aligned initramfs placement with an overlap and oversize guard.
- A `scripts/build-initramfs.sh` that builds a static busybox from source into
  `testdata/initramfs.cpio.gz`, plus the extra guest kernel config symbols.
- pty-based unit tests and a KVM-gated interactive end-to-end test.

### Out of Scope

- The event-loop substrate itself (vCPU thread, epoll I/O thread, `exit_evt`
  shutdown protocol, the `irqfd` delivery mechanism). Owned by
  [[0001-event-loop-and-concurrency-model]]; this work consumes it.
- Any virtio device, the MMIO bus, networking, SSH, and persistence.
- SMP, bzImage, ACPI, MPTable, snapshot, jailer, and any control API.
- A second emulated device. The UART stays the only device; it gains input.

## Current State

The UART is output-only. `serial::create` in
`crates/naos-linux/src/serial.rs` builds a
`Serial<EventFdTrigger, NoEvents, Stdout>`; guest THR writes reach
`serial::handle_write` (which flushes stdout) and status reads reach
`serial::handle_read`. `EventFdTrigger` wraps a `vmm_sys_util::eventfd::EventFd`
and implements `vm_superio::Trigger`, but its own doc comment notes it "exists
only to satisfy the type bound," because no serial interrupt is ever delivered
and nothing reads stdin.

`crates/naos-linux/src/vcpu.rs` runs a single-threaded, synchronous `KVM_RUN`
loop: `IoOut`/`IoIn` in the COM1 range (0x3F8..0x3FF) dispatch to the serial
handlers, a reset request (`reboot=k` on port 0x64, or the PCI reset register)
breaks the loop, and `Hlt`/`Shutdown` exit cleanly. The receive path already
exists on the guest side — an `IoIn` at COM1 offset 0 calls `serial.read(0)`,
draining a FIFO byte — but nothing ever fills that FIFO.

`crates/naos-linux/src/vmm.rs` (`Vmm::new`) creates the in-kernel IRQ chip
(`create_irq_chip`) and PIT, builds and registers guest memory
(`memory::build` / `memory::register`, a single region at guest physical 0),
loads the kernel (`kernel::load` via linux-loader's ELF loader), writes the
cmdline and zero page (`boot::write_cmdline`, `boot::write_boot_params` — e820
plus cmdline pointer, no ramdisk fields), then creates the serial device and
vCPU. `crates/naos-linux/src/main.rs` exposes `--kernel`, `--mem`, and
`--cmdline` (default `console=ttyS0 reboot=k panic=1 pci=off`). The boot-to-panic
e2e in `crates/naos-linux/tests/boot_e2e.rs` asserts the kernel banner,
`No working init found`, and exit 0.

## Dependencies

- **[[0001-event-loop-and-concurrency-model]] (hard blocker for Phase 2).** The
  stdin to UART receive path needs the event-loop thread from IMPL 0001: host
  stdin registered as a subscriber on the `event-manager` epoll loop, so that on
  readiness a handler reads stdin and calls `enqueue_raw_bytes` while the vCPU is
  blocked in `KVM_RUN` on its own thread. The receive interrupt is delivered by
  writing the serial `EventFd`, which IMPL 0001's `irqfd` registration turns into
  a guest IRQ. Phase 1 (raw terminal mode), Phase 3 (cmdline and initramfs), and
  Phase 4 (guest rootfs) do not touch the event loop and can land before IMPL
  0001; Phase 2 and the interactive e2e in Phase 5 must land after it.
- **Guest kernel config.** The `tinyconfig`-derived kernel from
  `scripts/build-test-kernel-x86_64.sh` must gain `CONFIG_BLK_DEV_INITRD`,
  `CONFIG_RD_GZIP`, `CONFIG_DEVTMPFS`, `CONFIG_DEVTMPFS_MOUNT`, `CONFIG_PROC_FS`,
  and `CONFIG_SYSFS`.
- **Guest rootfs toolchain.** A busybox source tree and a C toolchain able to
  produce a static binary, plus `cpio` and `gzip`, to build
  `testdata/initramfs.cpio.gz` (a generated, not committed, artifact like
  `testdata/vmlinux`).
- **Crates.** No new runtime crate for this work: raw-mode termios uses `libc`
  (already a dependency) and pty-based tests use `libc::openpty` (also `libc`).
  `event-manager` is introduced by IMPL 0001, not here; this work only registers
  a subscriber and an `irqfd` against it. `VmFd::register_irqfd` and `EventFd`
  come from the existing `kvm-ioctls` and `vmm-sys-util` dependencies.

## Implementation Phases

Each phase keeps the build green and preserves the no-initramfs boot-to-panic
path. Phases 1, 3, and 4 are independent of IMPL 0001; Phase 2 (and the
interactive e2e in Phase 5) require it.

### Phase 1: Raw terminal mode

Put host stdin into raw mode so keystrokes pass through unbuffered and unechoed,
with a RAII guard that always restores the terminal. This phase touches no KVM
state and can land ahead of IMPL 0001.

#### Tasks

- [ ] Add a `term` module (`crates/naos-linux/src/term.rs`) with a
  `RawTermGuard` that snapshots stdin's `termios` via `libc::tcgetattr`, builds
  raw attributes with `libc::cfmakeraw` (clearing `ICANON`, `ECHO`, `ISIG`,
  `IEXTEN`, `ICRNL`), and applies them with `libc::tcsetattr(TCSANOW)`.
- [ ] Store the saved `termios` in the guard and restore it in `Drop`, so the
  terminal returns to cooked mode on success, on error return, and on panic
  unwind.
- [ ] Guard on `libc::isatty(STDIN_FILENO)`: when stdin is a pipe or file, skip
  raw mode and return an inert guard so tests and CI can script input.
- [ ] Register the `term` module in `crates/naos-linux/src/main.rs` and leave
  stdout passthrough (the existing `Stdout` sink and per-write flush in
  `serial::handle_write`) unchanged.
- [ ] Add unit tests (against a pty from `libc::openpty`) asserting the guard
  restores the original `termios` on drop and that the non-TTY path is a no-op.

#### Success Criteria

- `cargo build -p naos-linux` and `cargo clippy -- -D warnings` pass.
- After a run over a real TTY (or a forced panic mid-run), the terminal is left
  in cooked mode (echo and line editing restored).
- The pty unit test confirms the saved and restored `termios` match.

### Phase 2: Host stdin to UART receive

Wire host stdin into the UART receive FIFO and deliver the receive interrupt to
the guest, making `EventFdTrigger` live. This phase consumes the event-loop
substrate and the `irqfd` delivery mechanism from
[[0001-event-loop-and-concurrency-model]], so it must land after IMPL 0001.

#### Tasks

- [ ] Add `SERIAL_GSI: u32 = 4` and a serial `irqfd` registration helper in
  `crates/naos-linux/src/serial.rs` that `try_clone`s the trigger's inner
  `EventFd` and calls `VmFd::register_irqfd(&event_fd, SERIAL_GSI)`, so
  vm-superio pulsing the trigger injects IRQ 4.
- [ ] Add an RX entry point in `serial.rs` wrapping
  `Serial::enqueue_raw_bytes(&buf)` (taken under the shared `Arc<Mutex<...>>`
  from IMPL 0001), which appends to the RX FIFO and pulses the trigger when the
  guest has enabled the receive-data interrupt in IER.
- [ ] Implement a `Ctrl-a x` escape state machine on the host RX read path: on
  `Ctrl-a` (0x01) enter a pending state; a following `x` requests shutdown
  (signal IMPL 0001's `exit_evt`), a following `Ctrl-a` forwards one literal
  0x01 to the guest, and any other byte forwards both.
- [ ] Register stdin as a subscriber on IMPL 0001's `event-manager` loop whose
  handler reads available bytes, feeds them through the escape state machine,
  and calls the RX entry point.
- [ ] Thread the serial `irqfd` registration and stdin subscriber into
  `Vmm::new` (`crates/naos-linux/src/vmm.rs`) after the serial device is created,
  and install the `RawTermGuard` before the vCPU thread is spawned.
- [ ] Add unit tests: `enqueue_raw_bytes` then `handle_read` at offset 0 returns
  the bytes in order; enqueuing with the RX interrupt enabled pulses the inner
  `EventFd`; the escape state machine maps `Ctrl-a x`, `Ctrl-a Ctrl-a`, and a
  plain byte correctly.

#### Success Criteria

- `cargo build -p naos-linux` and clippy pass; the boot-to-panic e2e still
  passes unchanged.
- With `/dev/kvm` available, typing into a running guest reaches it (the guest
  echoes input), and `Ctrl-a x` exits naos cleanly with status 0.
- The RX and escape state-machine unit tests pass.

### Phase 3: Interactive cmdline and initramfs load

Add the `--initramfs` flag, derive an interactive cmdline default, and load the
initramfs into guest RAM with `boot_params` wiring. Independent of IMPL 0001.

#### Tasks

- [ ] Add `initramfs: Option<PathBuf>` to `Args` in
  `crates/naos-linux/src/main.rs` and pass it to `Vmm::new` (new
  `initramfs: Option<&Path>` parameter).
- [ ] Derive the interactive cmdline default when `--initramfs` is set and
  `--cmdline` is not overridden: append `rdinit=/init`, drop `panic=1`, keep
  `reboot=k` (for example `console=ttyS0 reboot=k pci=off rdinit=/init`);
  otherwise keep the existing default so the boot-to-panic path is unchanged.
- [ ] Add an initramfs loader in `crates/naos-linux/src/kernel.rs` that reads the
  `.cpio.gz` and copies it into guest RAM top-down and page-aligned, returning
  `(GuestAddress, size)`, with a guard rejecting an image that overlaps the
  kernel or exceeds guest RAM.
- [ ] Extend `boot::write_boot_params` in `crates/naos-linux/src/boot.rs` to take
  the optional `(addr, size)` and write `hdr.ramdisk_image` / `hdr.ramdisk_size`
  (low 32 bits) and `ext_ramdisk_image` / `ext_ramdisk_size` (high 32 bits);
  leave all four zero when no initramfs is given.
- [ ] Call the loader in `Vmm::new` after `kernel::load` and pass the result into
  `write_boot_params`.
- [ ] Add unit tests: with an initramfs the ramdisk fields hold the expected
  low/high split (including `ext_*` for a synthetic address above 4 GiB); with no
  initramfs all four ramdisk fields are zero (a boot-to-panic regression guard);
  the placement guard rejects an oversize or overlapping image.

#### Success Criteria

- `cargo build -p naos-linux` and clippy pass; both e2e paths compile.
- With no `--initramfs`, the cmdline, `boot_params`, and boot-to-panic behavior
  are byte-for-byte unchanged.
- The boot_params and placement-guard unit tests pass.

### Phase 4: Guest busybox rootfs

Build a static busybox initramfs from source so the kernel has a userspace to
hand control to. Independent of IMPL 0001.

#### Tasks

- [ ] Add `scripts/build-initramfs.sh` (mirroring
  `scripts/build-test-kernel-x86_64.sh`) that builds a static busybox from
  source, installs its applet symlinks, and packs a cpio image into
  `testdata/initramfs.cpio.gz`.
- [ ] Include a minimal `/init` shell script that mounts `proc` and `sysfs` and
  execs `sh` on the console, plus a baked `/dev/console` node as a devtmpfs
  fallback.
- [ ] Extend the `./scripts/config` block in
  `scripts/build-test-kernel-x86_64.sh` with `CONFIG_BLK_DEV_INITRD`,
  `CONFIG_RD_GZIP`, `CONFIG_DEVTMPFS`, `CONFIG_DEVTMPFS_MOUNT`, `CONFIG_PROC_FS`,
  and `CONFIG_SYSFS`.
- [ ] Add a `just initramfs` recipe (and document it in `DEVELOPMENT.md`
  alongside the kernel-build instructions), keeping `testdata/initramfs.cpio.gz`
  a generated, not committed, artifact.
- [ ] Verify the artifact boots by hand: `just run --initramfs
  testdata/initramfs.cpio.gz` reaches a shell prompt where `ls` and `uname -a`
  produce plausible output and `poweroff` exits 0.

#### Success Criteria

- `just initramfs` produces `testdata/initramfs.cpio.gz` from source.
- The rebuilt kernel unpacks the initramfs and runs `/init`, giving a shell on
  `ttyS0`.
- `poweroff` in that shell drives the reset path to a clean exit 0.

### Phase 5: Tests for input and interactive boot

Add automated coverage for the receive path, the escape handling, and the full
interactive boot. The interactive e2e requires IMPL 0001 and `/dev/kvm`.

#### Tasks

- [ ] Add pty-based unit tests (via `libc::openpty`) for the RX enqueue,
  interrupt pulse, and `Ctrl-a x` state machine (co-located with Phase 2 code).
- [ ] Add an interactive e2e (a new `crates/naos-linux/tests/interactive_e2e.rs`
  or an addition to `boot_e2e.rs`), KVM-gated and skipping cleanly when
  `/dev/kvm` or `testdata/initramfs.cpio.gz` is absent, wrapped in `timeout`.
- [ ] In that test, script stdin (expect-style): wait for the shell prompt, send
  `ls` and `uname -a`, assert plausible output, send `poweroff`, and assert
  exit 0.
- [ ] Keep the existing no-initramfs boot-to-panic e2e passing unchanged as a
  regression guard.
- [ ] Confirm `just test-linux` (and the coverage recipes) run green with the new
  tests, skipping the KVM-gated ones where KVM is unavailable.

#### Success Criteria

- `cargo test -p naos-linux` passes with KVM absent (KVM-gated tests skip).
- With `/dev/kvm` and the initramfs present, the interactive e2e types commands,
  observes output, and exits 0.
- The boot-to-panic e2e continues to pass.

## Open Questions

Design decisions (initramfs placement strategy, interactive cmdline derivation,
the `Ctrl-a x` escape, and building busybox from source) are settled by the
source design. These are implementation-level details only.

### 1. Termios save and restore and panic safety

- **a** (recommended) — A `RawTermGuard` that saves `termios` in a field and
  restores it in `Drop`. Drop runs on scope exit, on `?` error propagation, and
  on panic unwind (naos uses the default unwinding panic strategy), so the
  terminal is always restored with no extra machinery.
- **b** — Explicit save and restore at the top and bottom of `run`, plus a panic
  hook to cover unwinding. More moving parts and easy to leak on an early return.
- **other** — *write-in*

**Decision:** a — a `RawTermGuard` that saves `termios` and restores it in `Drop`, covering scope exit, `?` propagation, and panic unwind.

### 2. Detecting the Ctrl-a x escape sequence

- **a** (recommended) — A small byte-level state machine on the host RX read
  path, before `enqueue_raw_bytes`, that consumes `Ctrl-a` (0x01) and dispatches
  on the next byte (`x` quits, `Ctrl-a` forwards one literal 0x01, anything else
  forwards both). This mirrors QEMU and, because it lives on the host input path,
  never inspects guest output. With `ISIG` cleared, a lone `Ctrl-a` is otherwise
  a harmless byte to the guest.
- **b** — A dedicated hotkey outside the byte stream (for example a signal or a
  second control fd). More plumbing and no longer in-band, contradicting the
  design's QEMU-style choice.
- **other** — *write-in*

**Decision:** a — a byte-level state machine on the host RX path, before `enqueue_raw_bytes`, that consumes `Ctrl-a` and dispatches on the next byte.

### 3. Where the initramfs lands in the current memory map

- **a** (recommended) — Compute the load address top-down from the top of the
  single guest RAM region that `memory::build` allocates at guest physical 0,
  page-aligned, and reject (fail `Vmm::new`) any image that would overlap the
  kernel (loaded near 16 MiB) or run past the top of RAM. For small default RAM
  this keeps the whole image below 4 GiB, so `ext_ramdisk_*` stay zero.
- **b** — A fixed low load address just above the kernel image (Firecracker
  style). Simpler math but fragile for larger images and closer to the kernel's
  own BSS.
- **other** — *write-in*

**Decision:** a — compute the load address top-down from the top of the single guest RAM region, page-aligned, and reject an image that overlaps the kernel or runs past RAM.

### 4. Producing a statically linked busybox

- **a** (recommended) — Build busybox with the host glibc toolchain and
  `--static` linking flags (`CONFIG_STATIC=y`), which needs only the tools the
  kernel build already assumes. Falls back to a musl cross-toolchain only if
  glibc static linking proves unreliable.
- **b** — Build against a musl toolchain from the start for a cleanly static
  binary, at the cost of an extra toolchain dependency in `DEVELOPMENT.md`.
- **other** — *write-in*

**Decision:** a — build busybox with the host glibc toolchain and `--static` (`CONFIG_STATIC=y`), falling back to a musl cross-toolchain only if glibc static linking proves unreliable.

## File Changes

| File | Action | Description |
|------|--------|-------------|
| `crates/naos-linux/src/term.rs` | Create | `RawTermGuard`: TTY-gated raw-mode termios guard that restores on drop, with pty unit tests. |
| `crates/naos-linux/src/serial.rs` | Modify | `SERIAL_GSI = 4`, an `irqfd` registration helper, an `enqueue_raw_bytes` RX entry point, and the `Ctrl-a x` escape state machine, with tests. |
| `crates/naos-linux/src/vmm.rs` | Modify | `Vmm::new` gains `initramfs: Option<&Path>`; registers the serial `irqfd` on GSI 4 and the stdin subscriber; installs `RawTermGuard`; loads the initramfs and passes it to `write_boot_params`. |
| `crates/naos-linux/src/main.rs` | Modify | Add `--initramfs`, derive the interactive cmdline default, register the `term` module. |
| `crates/naos-linux/src/kernel.rs` | Modify | Initramfs loader with top-down, page-aligned placement and an overlap/oversize guard, with tests. |
| `crates/naos-linux/src/boot.rs` | Modify | `write_boot_params` writes `hdr.ramdisk_image`/`ramdisk_size` and `ext_ramdisk_image`/`ext_ramdisk_size`; zero when no initramfs. |
| `crates/naos-linux/tests/interactive_e2e.rs` | Create | KVM-gated interactive e2e: prompt, `ls`, `uname -a`, `poweroff`, exit 0. |
| `crates/naos-linux/tests/boot_e2e.rs` | Modify | Keep the no-initramfs boot-to-panic case as a regression guard. |
| `scripts/build-initramfs.sh` | Create | Build a static busybox from source and pack `testdata/initramfs.cpio.gz`. |
| `scripts/build-test-kernel-x86_64.sh` | Modify | Add `CONFIG_BLK_DEV_INITRD`, `CONFIG_RD_GZIP`, `CONFIG_DEVTMPFS`, `CONFIG_DEVTMPFS_MOUNT`, `CONFIG_PROC_FS`, `CONFIG_SYSFS`. |
| `Justfile` | Modify | Add a `just initramfs` recipe. |
| `DEVELOPMENT.md` | Modify | Document the initramfs build and the extra kernel config symbols. |

## Testing Plan

- [ ] Unit: `RawTermGuard` restores the original `termios` on drop, and the
  non-TTY path is a no-op (pty via `libc::openpty`).
- [ ] Unit: `enqueue_raw_bytes` then `handle_read` at offset 0 returns bytes in
  order; enqueue with the RX interrupt enabled pulses the inner `EventFd`.
- [ ] Unit: the `Ctrl-a x` state machine maps `Ctrl-a x` (quit),
  `Ctrl-a Ctrl-a` (one literal 0x01), and plain bytes (passthrough) correctly.
- [ ] Unit: `write_boot_params` writes the correct low/high ramdisk split with an
  initramfs (including `ext_*` above 4 GiB) and all-zero ramdisk fields without.
- [ ] Unit: the initramfs placement guard rejects oversize and kernel-overlapping
  images.
- [ ] Integration (KVM-gated): serial `irqfd` registration on GSI 4 succeeds
  against a real `VmFd`.
- [ ] Integration (KVM-gated): interactive e2e types `ls` and `uname -a`, sees
  output, and `poweroff` exits 0.
- [ ] Integration (KVM-gated): the no-initramfs boot-to-panic e2e still exits 0.

## References

- [[0002-interactive-serial-console]] — source design (Detailed Design, Testing
  Strategy, and the decided Open Questions).
- [[0001-event-loop-and-concurrency-model]] — hard dependency: vCPU thread,
  epoll I/O loop, `exit_evt` shutdown, and `irqfd` delivery.
- [[0002-microvm-first-incremental-milestone-ladder]] — the M2 milestone.
- [[0003-event-driven-epoll-concurrency-model]] — the concurrency model consumed
  here.
- [[0005-root-filesystem-initramfs-then-virtio-blk]] — initramfs-first rootfs.
- `crates/naos-linux/src/serial.rs`, `vcpu.rs`, `vmm.rs`, `boot.rs`,
  `kernel.rs`, `memory.rs`, `main.rs` — current implementation.
- `crates/naos-linux/tests/boot_e2e.rs` — existing boot-to-panic e2e.
- `scripts/build-test-kernel-x86_64.sh`, `Justfile` — build story to extend.
- vm-superio `Serial` (`enqueue_raw_bytes`, `Trigger`), kvm-ioctls
  (`VmFd::register_irqfd`), vmm-sys-util (`EventFd`), `libc` termios
  (`tcgetattr`, `cfmakeraw`, `tcsetattr`, `isatty`, `openpty`).
- Linux/x86 boot protocol `Documentation/arch/x86/boot.rst`: `ramdisk_image`,
  `ramdisk_size`, `ext_ramdisk_image`, `ext_ramdisk_size`.
