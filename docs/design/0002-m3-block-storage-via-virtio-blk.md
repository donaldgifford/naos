---
id: DESIGN-0002
title: "M3 — block storage via virtio-blk"
status: Draft
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# DESIGN 0002: M3 — block storage via virtio-blk

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
  - [1. The MMIO bus and vCPU dispatch](#1-the-mmio-bus-and-vcpu-dispatch)
  - [2. The virtio-mmio transport](#2-the-virtio-mmio-transport)
  - [3. Virtqueues and the data plane](#3-virtqueues-and-the-data-plane)
  - [4. The virtio-blk backend](#4-the-virtio-blk-backend)
  - [5. Threading: config plane vs data plane](#5-threading-config-plane-vs-data-plane)
  - [6. Guest discovery, memory map, and IRQ routing](#6-guest-discovery-memory-map-and-irq-routing)
  - [7. Rootfs image and guest kernel](#7-rootfs-image-and-guest-kernel)
- [API / Interface Changes](#api--interface-changes)
- [Data Model](#data-model)
- [Testing Strategy](#testing-strategy)
- [Migration / Rollout Plan](#migration--rollout-plan)
- [Open Questions](#open-questions)
- [References](#references)
<!--toc:end-->

## Overview

M3 gives naos a persistent, disk-backed root filesystem. It builds the reusable
virtio foundation — an MMIO bus, the virtio-mmio transport, split virtqueues,
and a virtio-blk backend against a raw image file — so the guest boots from a
real ext4 disk (Alpine or minimal Debian) to a serial login instead of a RAM
initramfs. The transport, queue, and interrupt plumbing built here is the same
plumbing virtio-net will reuse at M4, so most of this milestone is foundation,
not block-specific code.

## Goals and Non-Goals

### Goals

- Handle `KVM_EXIT_MMIO` in the vCPU exit dispatch and route guest MMIO
  reads/writes to the device registered for the faulting address range.
- Implement the modern (virtio 1.x) **virtio-mmio transport** — register
  interface, device/driver feature negotiation, queue configuration, status
  handshake — per virtio 1.2 §4.2.2, over **split virtqueues** per §2.7.
- Implement a **virtio-blk** device that services read, write, and flush
  requests against a raw host image file (`std::fs::File`, `O_RDWR`), per
  virtio 1.2 §5.2, running the backend on the M2 event loop
  ([[0001-m2-interactive-serial-console]]).
- Signal completions to the guest through **irqfd** on a per-device GSI, and wake
  the backend on a guest queue-notify through **ioeventfd**, so the data plane
  never needs a full userspace MMIO round-trip.
- Add a `--drive <path>` CLI flag, produce a raw ext4 rootfs image, boot it with
  `root=/dev/vda`, and keep the M2 initramfs path working (block device optional).
- **Success criterion:** boot a disk-image rootfs to a serial login; write a
  file, reboot, and observe that the file persists.

### Non-Goals

- **virtio-pci or legacy virtio.** virtio-mmio only, per
  [[0004-virtio-over-mmio-device-transport]].
- **Multiple queues per device, indirect descriptors, or packed virtqueues.**
  One split virtqueue per block device; the extra feature bits stay negotiated
  off until a workload needs them.
- **Asynchronous / batched host I/O** (io_uring, request coalescing, write-back
  caching). The backend does straightforward synchronous `pread`/`pwrite` for
  now; performance work is deferred.
- **Multiple block devices, hotplug, resize, discard/TRIM, read-only images.**
  A single writable data disk is enough to prove the substrate; more devices
  fall out of the same transport later.
- **In-place guest reboot.** A guest reset still exits the VMM cleanly (M2
  behavior); "reboot" in the success criterion means relaunching naos against
  the same image (see Open Questions).
- **virtio-net.** That is M4 ([[0003-m4-guest-networking-and-ssh]]); it reuses
  everything here.

## Background

The MVP (`DESIGN-naos-linux.md`) boots a kernel to a panic with one PIO device
(the 16550 UART) and a single blocking vCPU loop. M2 introduced the pivotal
architectural change: an epoll-based event loop
([[0003-event-driven-epoll-concurrency-model]]) with the vCPU on its own thread
and host-side I/O sources — serial stdin, eventfds — serviced on readiness on a
separate I/O thread, with device interrupts delivered via KVM irqfd. M3 assumes
that machinery exists.

The rootfs ladder ([[0005-root-filesystem-initramfs-then-virtio-blk]]) puts
initramfs first (M2, RAM-only, zero device machinery) and virtio-blk second
(M3, persistent, disk-backed); both remain first-class. The transport is settled:
virtio-mmio, modern, no PCI ([[0004-virtio-over-mmio-device-transport]]). Each
device gets a fixed MMIO region and a dedicated IRQ line, discovered by the guest
from the kernel command line rather than by bus enumeration — Firecracker's
microVM model, minimal guest config. `WALK-linux.md` §13 flags virtio-blk as
"the single biggest unlock" and the first consumer of an MMIO bus and IRQ routing.

Today the vCPU dispatch in `vcpu.rs` handles only `IoOut`/`IoIn` (PIO), `Hlt`,
`Shutdown`, and a reset request; any MMIO exit falls through to the defensive
`bail!` arm (the `run_errors_on_an_unhandled_exit` test relies on exactly that).
`vmm.rs` already creates an in-kernel IRQ chip and PIT before vCPU creation, so
GSI routing through the IOAPIC is available for free. Guest RAM (`memory.rs`) is
one contiguous region from physical 0 with no MMIO hole; M3 introduces the first
MMIO devices.

## Detailed Design

The design has five parts: the MMIO bus that routes exits, the virtio-mmio
transport register model, the virtqueue data path, the virtio-blk backend, and
the guest-facing discovery/image story. The through-line is a clean split
between a low-frequency **config plane** (transport registers, serviced on the
vCPU thread) and a high-frequency **data plane** (queue notifications and
completions, serviced on the I/O thread without userspace MMIO exits).

### 1. The MMIO bus and vCPU dispatch

KVM reports a guest access to an unbacked physical address as
`VcpuExit::MmioRead(addr, data)` or `VcpuExit::MmioWrite(addr, data)`. Two new
arms are added to the `match` in `vcpu::run`:

```text
MmioRead(addr, data)  => bus.read(addr, data),   // fill `data` from device
MmioWrite(addr, data) => bus.write(addr, data),  // apply `data` to device
```

A small `MmioBus` owns an ordered map of `[base, base+len)` ranges to device
handles. `read`/`write` find the range containing `addr`, translate to a
register offset (`addr - base`), and dispatch to that device's transport. An
access that matches no range returns zeroes / is ignored, mirroring the existing
"return 0xFF for unknown PIO ports" convention — the guest probes addresses we
don't back and we must not crash. The bus is deliberately tiny; it is address
routing and nothing else. It replaces the current unconditional `bail!` for MMIO
exits, so that defensive test is updated to point at a still-unmapped address.

Note the boot-time identity page tables in `boot.rs` cover only the first 1 GiB,
and the virtio-mmio window lives above that (see the memory map below). This is
fine: those tables exist only to get the guest kernel to `startup_64`, after
which the kernel installs its own page tables and `ioremap`s device regions
before it ever touches them.

### 2. The virtio-mmio transport

The transport is the register file the guest driver in `drivers/virtio/`
programs to bring a device up. It implements the modern MMIO register layout
(virtio 1.2 §4.2.2). The load-bearing registers:

| Offset | Name | Access | Purpose |
| ------ | ---- | ------ | ------- |
| 0x000 | MagicValue | R | `0x74726976` ("virt", little-endian) |
| 0x004 | Version | R | `2` — modern, non-legacy |
| 0x008 | DeviceID | R | `2` for a block device (virtio 1.2 §5.2.2) |
| 0x00c | VendorID | R | naos vendor id (informational) |
| 0x010 / 0x014 | DeviceFeatures / Sel | R / W | 64-bit device feature bits, windowed |
| 0x020 / 0x024 | DriverFeatures / Sel | W / W | bits the driver accepts |
| 0x030 | QueueSel | W | select the queue subsequent regs address |
| 0x034 | QueueNumMax | R | max descriptors we support for this queue |
| 0x038 | QueueNum | W | size the driver chose |
| 0x044 | QueueReady | RW | 1 = queue live |
| 0x050 | QueueNotify | W | driver kicks the device (data-plane doorbell) |
| 0x060 / 0x064 | InterruptStatus / ACK | R / W | pending-interrupt bits / ack |
| 0x070 | Status | RW | device-status handshake byte |
| 0x080–0x0a4 | Queue{Desc,Driver,Device}{Low,High} | W | guest addresses of the three rings |
| 0x0fc | ConfigGeneration | R | config-space consistency counter |
| 0x100+ | Config | R(W) | device-specific config (blk capacity, etc.) |

**Feature negotiation** (virtio 1.2 §2.2) is windowed through the `*Sel`
registers because the feature space is 64 bits wide and each data register is 32
bits. The device must advertise `VIRTIO_F_VERSION_1` (bit 32); for block we also
advertise a minimal set (e.g. `VIRTIO_BLK_F_FLUSH`) and negotiate everything
else off for now. The driver reads `DeviceFeatures` in both windows, writes the
intersection it accepts to `DriverFeatures`, then sets `FEATURES_OK`.

**Status handshake** (virtio 1.2 §2.1). The driver walks the `Status` byte
through `ACKNOWLEDGE (1)` → `DRIVER (2)` → `FEATURES_OK (8)` → `DRIVER_OK (4)`.
We validate the transitions, clear `FEATURES_OK` on an unacceptable feature set,
and set `DEVICE_NEEDS_RESET (64)` on a protocol error; a write of `0` resets the
device and tears the queues down.

Config-plane register reads/writes are infrequent (probe and reset), so they run
synchronously on the vCPU thread through the `MmioBus`. The state they touch is
shared with the I/O thread (part 5) behind a lock.

### 3. Virtqueues and the data plane

A split virtqueue (virtio 1.2 §2.7) is three structures the driver allocates in
guest RAM, whose guest-physical addresses it hands us via the `Queue*` registers:

- **Descriptor table** (§2.7.5): an array of `(addr, len, flags, next)`
  descriptors. Each describes one guest buffer; `VIRTQ_DESC_F_NEXT` chains them
  and `VIRTQ_DESC_F_WRITE` marks a device-writable (i.e. read-into-by-device)
  buffer.
- **Available ring** (§2.7.6): driver → device. The driver publishes the head
  descriptor index of each new request here and bumps `avail.idx`.
- **Used ring** (§2.7.8): device → driver. The device publishes completed head
  indices plus bytes-written here and bumps `used.idx`.

We do not hand-roll ring parsing. The rust-vmm `virtio-queue` crate provides
`Queue` and `DescriptorChain` iteration over any `vm-memory` `GuestMemory`; it
handles index wrapping, bounds checks against guest RAM, and used-ring
publication. Guest memory is shared with the I/O thread as a
`vm-memory` `GuestMemoryAtomic<GuestMemoryMmap>` so the backend can read/write
guest buffers safely while the vCPU thread runs.

The data-plane loop, end to end:

```text
guest driver: write head idx to avail ring, bump avail.idx,
              write queue index to QueueNotify (0x050)
        │
        ▼  KVM matches an ioeventfd on the QueueNotify address+datamatch:
           no userspace MMIO exit — KVM signals the eventfd and resumes the guest
        │
        ▼  I/O thread (event-manager epoll) wakes on that eventfd
backend:  Queue::iter() → for each DescriptorChain:
            parse virtio_blk_req header, do host I/O, write status byte,
            Queue::add_used(head, len)
        │
        ▼  set InterruptStatus bit 0 (used-buffer notification), then
           write 1 to the irqfd eventfd for this device's GSI
        │
        ▼  KVM injects the IRQ via the in-kernel IOAPIC; guest ISR reads
           InterruptStatus (MMIO exit → transport), drains the used ring,
           writes InterruptACK to clear the bit
```

The two eventfds are the crux of ADR-0004's "inject through irqfd" and the
reason the data plane is cheap:

- **ioeventfd** on QueueNotify. `kvm-ioctls` `VmFd::register_ioevent` binds an
  `EventFd` to the MMIO address `0x050` within the device window with a datamatch
  on the queue index. A guest write there fires the eventfd inside the kernel
  and the guest keeps running — no exit to `vcpu::run`, no `MmioBus` dispatch.
  The eventfd is registered with the event loop so the backend wakes on it.
- **irqfd** on the device GSI. `kvm-ioctls` `VmFd::register_irqfd` binds an
  `EventFd` to the device's GSI. The backend injects an interrupt by writing to
  it; KVM and the already-created in-kernel IRQ chip do the rest. No
  `KVM_IRQ_LINE` ioctl per completion.

### 4. The virtio-blk backend

virtio-blk (virtio 1.2 §5.2) carries each request as a descriptor chain of at
least three segments:

1. A device-readable header, `struct virtio_blk_req` (§5.2.6): `type` (u32),
   `reserved` (u32), `sector` (u64). Type is `VIRTIO_BLK_T_IN (0)` read,
   `VIRTIO_BLK_T_OUT (1)` write, or `VIRTIO_BLK_T_FLUSH (4)`. `sector` is a
   512-byte LBA regardless of the host file's block size.
2. One or more data segments — device-writable for reads, device-readable for
   writes.
3. A one-byte device-writable status: `VIRTIO_BLK_S_OK (0)`,
   `VIRTIO_BLK_S_IOERR (1)`, or `VIRTIO_BLK_S_UNSUPP (2)`.

The backend owns the image as a `std::fs::File` opened `O_RDWR`. For each chain:

- **IN**: `read_exact_at(buf, sector * 512)` into the guest data buffers, write
  `S_OK`, and report the number of bytes written to the used ring.
- **OUT**: gather the guest data buffers and `write_all_at(buf, sector * 512)`,
  write `S_OK`.
- **FLUSH**: `File::sync_all()` (fsync) to guarantee durability, write `S_OK`.
  Flush is what makes the success criterion hold: the guest's `sync`/unmount on
  shutdown reaches the host file before the VMM exits.

Any host I/O error or malformed chain yields `S_IOERR` (or `S_UNSUPP` for an
unknown type) rather than aborting the VM. Bounds are checked against the image
length reported as `capacity` in the block config space (§5.2.4, in 512-byte
sectors) so a guest cannot read or write outside the image.

### 5. Threading: config plane vs data plane

The device is one object, `VirtioBlk`, holding the transport register state, the
`Queue`, the backing `File`, and clones of the two eventfds. It is shared as an
`Arc<Mutex<…>>` between:

- the **vCPU thread**, which reaches it through the `MmioBus` for config-plane
  register reads/writes, and
- the **I/O thread**, which reaches it as an event-manager subscriber that runs
  the data-plane loop when the QueueNotify ioeventfd fires.

`InterruptStatus` is the one field touched from both sides at data-plane rate; it
is an `AtomicU32` so the ISR-side read (vCPU thread) and the set-on-completion
(I/O thread) don't contend on the mutex. The exact locking granularity — one
mutex for the whole device vs. a lock-free ring plus a mutex only for config — is
in Open Questions. The rust-vmm building blocks are listed under References.

### 6. Guest discovery, memory map, and IRQ routing

There is no PCI bus, so the guest cannot probe for the device; it is told where
to look on the kernel command line (virtio 1.2 §4.2.2, and Linux's
`drivers/virtio/virtio_mmio.c` module parameter):

```text
virtio_mmio.device=<size>@<addr>:<irq>
# e.g.  virtio_mmio.device=0x1000@0xd0000000:5
```

This maps a single virtio-mmio window and wires its interrupt. The VMM and the
cmdline must agree on the triple. Proposed fixed layout (a hardcoded decision,
per ADR-0004's "opinions, not options"; exact values in Open Questions):

```text
Guest physical address space (additions for M3)
 …
 0x1000_0000   top of a 256 MiB guest RAM region (memory.rs)
 …             (unbacked — MMIO accesses here vmexit)
 0xd000_0000   virtio-mmio device 0  ── 0x1000 (one page) ── GSI 5
 0xd000_1000   virtio-mmio device 1  ── 0x1000            ── GSI 6   (future)
 …
```

The window sits far above guest RAM, so accesses to it always trap as MMIO and
never collide with a memory slot; no MMIO hole in the RAM region is required.
`memory.rs` is unchanged for a 256 MiB guest. The GSI is a real IOAPIC input on
the in-kernel IRQ chip `vmm.rs` already creates, so no new IRQ infrastructure is
needed — only the `register_irqfd` binding.

`Vmm::new` gains, after the existing IRQ-chip/PIT/memory steps and before the
run loop: open the drive file (if `--drive` was given), construct `VirtioBlk`,
register its ioeventfd and irqfd with the `VmFd`, insert it into the `MmioBus`,
register it as an event-loop subscriber, and append the `virtio_mmio.device=…`
clause plus `root=/dev/vda rw` to the cmdline. When no drive is given, none of
this happens and the M2 initramfs path runs exactly as before.

### 7. Rootfs image and guest kernel

**Image.** A raw file holding a single ext4 filesystem — no partition table, so
the whole disk is the filesystem and the guest mounts `/dev/vda` directly. Built
from Alpine's `minirootfs` tarball or Debian `debootstrap`, unpacked into a
mounted loopback image (`truncate` → `mkfs.ext4 -F` → mount → extract → set up a
serial getty on `ttyS0` → unmount). The image lives outside the repo;
`DEVELOPMENT.md`/`Justfile` grow a recipe, mirroring how the test vmlinux is
produced today.

**Cmdline.** `root=/dev/vda rw console=ttyS0` plus the `virtio_mmio.device=…`
clause. The M2 defaults (`reboot=k panic=1 pci=off`) stay.

**Guest kernel config deltas** on top of the M2 config:

- `CONFIG_VIRTIO=y` — core virtio.
- `CONFIG_VIRTIO_MMIO=y` — the MMIO transport and the `virtio_mmio.device=`
  cmdline parser.
- `CONFIG_VIRTIO_BLK=y` — the block driver that exposes `/dev/vda`.
- `CONFIG_EXT4_FS=y` — mount the ext4 rootfs (built-in, not a module, since
  there is no initramfs to load a module from on the disk-boot path).

## API / Interface Changes

- **New CLI flag** on the `naos-linux` binary:

  ```text
  naos-linux --kernel <PATH> --mem <MIB> [--cmdline <STR>]
             [--initrd <PATH>] [--drive <PATH>]
  ```

  `--drive <PATH>` points at a raw disk image opened `O_RDWR`. It is optional; if
  omitted the VM boots exactly as at M2 (initramfs or bare, per that milestone).
  `--drive` and an initramfs may coexist. (`--rootfs` is a candidate alias — see
  Open Questions.)
- **Cmdline injection.** When `--drive` is present, naos appends
  `virtio_mmio.device=<size>@<addr>:<irq>` and `root=/dev/vda rw` to the kernel
  command line so the guest discovers and mounts the disk. Users may still pass
  an explicit `--cmdline`; the virtio clause is appended, not overridden.
- **Internal module surface** (new): an `mmio` module (`MmioBus`) and a `virtio`
  module (transport + `virtio-queue` glue + `VirtioBlk`). `vcpu::run` gains
  `MmioRead`/`MmioWrite` arms and takes the bus. No change to the PIO, `Hlt`, or
  reset behavior.

## Data Model

- **On-disk:** a raw image file, no partition table, one ext4 filesystem. naos
  treats it as an opaque array of 512-byte logical sectors; the ext4 layout is
  the guest's concern. Capacity is `file_len / 512`, reported through the block
  config space (virtio 1.2 §5.2.4).
- **In guest memory:** the driver allocates the descriptor table, available ring,
  per-request `virtio_blk_req` headers, and data buffers (virtio 1.2 §2.7,
  §5.2.6), which naos reads; naos writes the used ring, status bytes, and
  read-data buffers. naos never allocates guest-side structures.
- **In VMM memory:** the transport register file (feature windows, queue
  addresses/size/ready, `Status`, `AtomicU32` `InterruptStatus`), the
  `virtio-queue` `Queue` state, the backing `File`, and the ioeventfd/irqfd
  `EventFd`s. Nothing is persisted across runs except the image file.

## Testing Strategy

Following the house pattern (`DESIGN-naos-linux.md`): unit tests where the logic
is fiddly and easy to get wrong, and one end-to-end check that exercises the
whole stack.

- **Transport register unit tests** (no KVM): drive the register model directly —
  MagicValue/Version/DeviceID read back correct constants; the feature-select
  windows expose the 64-bit feature space correctly; the `Status` handshake only
  advances through the legal `ACKNOWLEDGE→DRIVER→FEATURES_OK→DRIVER_OK` sequence
  and a `0` write resets. These are pure functions of the register file, like the
  existing GDT/e820 byte-layout tests in `boot.rs`.
- **virtqueue / backend unit tests** (no KVM): back a `GuestMemoryMmap` with
  hand-built descriptor chains and a temp image file; assert the backend parses
  IN/OUT/FLUSH, writes the right status byte, lands bytes at `sector * 512` in the
  host file, publishes the right `(head, len)` to the used ring, and rejects
  malformed chains (`S_IOERR`), unknown types (`S_UNSUPP`), and out-of-range LBAs.
- **MMIO bus routing** (no KVM): adjacent ranges dispatch to the right device and
  an unmapped address is a no-op, not a panic.
- **KVM-gated tests** follow the existing skip-cleanly-without-`/dev/kvm`
  convention: assert `register_ioevent`/`register_irqfd` succeed against a real
  `VmFd`.
- **End-to-end acceptance:** `naos-linux --kernel <vmlinux> --drive rootfs.img`
  boots to a serial login; log in, `echo hi > /root/persist`, `sync`,
  `poweroff`; relaunch against the same image; the file is still there. This is
  the M3 success criterion and the single test that proves the whole path.

## Migration / Rollout Plan

Incremental, each step independently observable, so a regression is bisectable to
one commit — the same "each milestone de-risks the next" discipline as the ADR
ladder ([[0002-microvm-first-incremental-milestone-ladder]]):

1. **MMIO bus + dispatch.** Add `MmioBus` and the `MmioRead`/`MmioWrite` arms.
   With no device registered, behavior is unchanged (unmapped MMIO is a no-op);
   the M2 boot still works. Update the `run_errors_on_an_unhandled_exit` test to
   target a still-unmapped address.
2. **Transport shell.** Register a virtio-mmio window whose DeviceID/feature/
   status registers respond, but with no real queue. Boot the M2 initramfs kernel
   with `CONFIG_VIRTIO_MMIO=y`; confirm the guest *probes* the device (visible in
   dmesg) without crashing. No functional block device yet.
3. **Queues + ioeventfd/irqfd wiring.** Bring up the `Queue`, register the two
   eventfds, subscribe the backend to the event loop. Verify a guest queue-notify
   wakes the backend and an injected interrupt reaches the guest ISR.
4. **virtio-blk data path.** Implement IN/OUT/FLUSH against the image file. Boot
   with `--drive` and `root=/dev/vda`; reach a serial login on the disk rootfs.
5. **Persistence + polish.** Confirm the write/reboot/persist criterion, add the
   image-build recipe and kernel-config deltas to `DEVELOPMENT.md`/`Justfile`.

The initramfs path is never removed. `--drive` is additive; omitting it yields
byte-for-byte M2 behavior, so M3 cannot regress M2.

## Open Questions

- **Exact MMIO base, per-device stride, and GSI.** `0xd0000000`, `0x1000`, and
  GSI 5 are reasonable (Firecracker-adjacent) defaults but should be confirmed
  against the IOAPIC's available GSIs and the guest kernel's expectations before
  they are hardcoded — the same "confirm the addresses" caveat the MVP raised for
  the GDT/page-table placement.
- **Flag naming.** `--drive` vs `--rootfs`, and how a future second disk is
  spelled (repeatable `--drive`, an index suffix, a small spec string). Decide
  when wiring `clap`.
- **Locking granularity.** One `Mutex` over the whole `VirtioBlk` vs. a
  finer split (config behind a mutex, the ring lock-free, `InterruptStatus`
  atomic). Start coarse; revisit if the vCPU thread contends with the I/O thread.
- **`virtio-device` / `virtio-queue` API surface.** The exact traits to
  implement (`VirtioDevice` and the MMIO transport helper) and how much of the
  register decode those crates provide vs. what naos writes by hand — pin down
  against the published crate versions before coding, rather than assuming an
  API here.
- **In-place reboot.** M3 treats a guest reset as a clean VMM exit (M2 behavior),
  so "reboot" in the success criterion is a relaunch. Real in-guest reboot
  (reset the vCPU and device state, re-run without exiting) is deferred; is a
  relaunch acceptable for the demo, or should M3 do a true reset?
- **Write durability policy.** Honor guest `FLUSH` only (fast, correct for a
  well-behaved guest), or also `O_DIRECT`/periodic `fsync` for safety against
  host crashes? Tied to whether we ever advertise a writeback-cache feature.
  Raw image only for M3; qcow2 is its own subsystem and out of scope.

## References

- ADRs: [[0002-microvm-first-incremental-milestone-ladder]],
  [[0003-event-driven-epoll-concurrency-model]],
  [[0004-virtio-over-mmio-device-transport]],
  [[0005-root-filesystem-initramfs-then-virtio-blk]]
- Sibling designs: [[0001-m2-interactive-serial-console]] (prerequisite event
  loop + irqfd), [[0003-m4-guest-networking-and-ssh]] (reuses this transport)
- `DESIGN-naos-linux.md` (MVP scope, house style); `WALK-linux.md` §2 (memory
  map), §13 (What's next); current code in `crates/naos-linux/src/`: `vcpu.rs`
  (exit dispatch), `vmm.rs` (init order, IRQ chip + PIT), `memory.rs`, `boot.rs`
- Virtio 1.2 spec: §2.1 (device status), §2.2 (feature bits), §2.7 (split
  virtqueues), §4.2.2 (virtio-mmio register layout), §5.2 / §5.2.6 (block device
  and `virtio_blk_req` format)
- Guest kernel config: `CONFIG_VIRTIO`, `CONFIG_VIRTIO_MMIO`, `CONFIG_VIRTIO_BLK`,
  `CONFIG_EXT4_FS`
- rust-vmm crates: `virtio-device`, `virtio-queue`, `vm-memory`
  (`GuestMemoryAtomic`), `kvm-ioctls` (`register_ioevent` / `register_irqfd`),
  `event-manager`, `vmm-sys-util` (`EventFd`)
