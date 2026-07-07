---
id: DESIGN-0004
title: "Block storage via virtio-blk"
status: Draft
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# DESIGN 0004: Block storage via virtio-blk

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
  - [1. Where the block device plugs into the transport](#1-where-the-block-device-plugs-into-the-transport)
  - [2. Block config space: capacity](#2-block-config-space-capacity)
  - [3. The request format and its descriptor chain](#3-the-request-format-and-its-descriptor-chain)
  - [4. Servicing read, write, and flush](#4-servicing-read-write-and-flush)
  - [5. Wiring the drive into the VMM](#5-wiring-the-drive-into-the-vmm)
  - [6. Rootfs image and guest kernel](#6-rootfs-image-and-guest-kernel)
- [API / Interface Changes](#api--interface-changes)
- [Data Model](#data-model)
- [Testing Strategy](#testing-strategy)
- [Migration / Rollout Plan](#migration--rollout-plan)
- [Open Questions](#open-questions)
  - [1. Drive flag naming and multi-disk shape](#1-drive-flag-naming-and-multi-disk-shape)
  - [2. In-place reboot versus relaunch](#2-in-place-reboot-versus-relaunch)
  - [3. Write durability policy](#3-write-durability-policy)
  - [4. Host I/O engine for network-backed storage](#4-host-io-engine-for-network-backed-storage)
- [References](#references)
<!--toc:end-->

## Overview

This design gives naos a persistent, disk-backed root filesystem by implementing
a **virtio-blk** device (virtio 1.2 §5.2) on top of the virtio-mmio substrate
([[0003-virtio-mmio-device-model]]). The device services guest read, write, and
flush requests against a raw ext4 host image file, so the guest boots from a real
disk to a serial login and its writes survive across runs. This doc is only the
block-device layer; the MMIO bus, transport register file, virtqueues, and
irqfd/ioeventfd plumbing live in the substrate and are referenced, not re-explained.

## Goals and Non-Goals

### Goals

- Implement a **virtio-blk** device (virtio 1.2 §5.2) against the substrate's
  device interface: report block `DeviceID` 2, advertise the block config space,
  and service one split virtqueue of requests.
- Serve the block **config space** — disk capacity in 512-byte sectors (virtio 1.2
  §5.2.4) — from the backing file's length.
- Parse the **request format** (virtio 1.2 §5.2.6: `type` / `sector` / `status`)
  and service `VIRTIO_BLK_T_IN`, `VIRTIO_BLK_T_OUT`, and `VIRTIO_BLK_T_FLUSH`
  against a raw host image (`std::fs::File`, `O_RDWR`, `pread`/`pwrite`), with the
  backend on the event loop ([[0001-event-loop-and-concurrency-model]]) and
  completions signalled through the transport's irqfd.
- Add a `--drive <path>` CLI flag, produce a raw ext4 rootfs image, and boot it
  with `root=/dev/vda`, while keeping the initramfs path
  ([[0002-interactive-serial-console]]) working (the block device is optional).
- **Success criterion:** boot from the disk image to a serial login; write a file,
  reboot (relaunch naos against the same image), and observe the file persists.

### Non-Goals

- **Transport, bus, and interrupt plumbing.** The MMIO bus, virtio-mmio register
  file, split-virtqueue parsing, feature-negotiation windows, status handshake,
  ioeventfd doorbell, and irqfd injection are the substrate's job
  ([[0003-virtio-mmio-device-model]]). This doc consumes that interface.
- **Multiple queues.** One split virtqueue per block device; no multiqueue
  (`VIRTIO_BLK_F_MQ` stays off).
- **Asynchronous or batched host I/O for the MVP.** Synchronous `pread`/`pwrite`;
  no request coalescing or write-back caching. An io_uring async engine is the
  planned enhancement for network-backed storage — see Open Questions.
- **Rich image formats.** Raw images only — no qcow2, which is its own subsystem.
- **Multiple disks, hotplug, resize, read-only images, discard/TRIM.** A single
  writable disk proves persistence; the extras (`VIRTIO_BLK_F_DISCARD`,
  `VIRTIO_BLK_F_RO`, and friends) stay off until a workload needs them.
- **In-place guest reboot.** A guest reset still exits the VMM cleanly (prior
  milestone behavior); "reboot" in the success criterion means relaunching naos
  against the same image (see Open Questions).

## Background

The rootfs ladder ([[0005-root-filesystem-initramfs-then-virtio-blk]]) puts
initramfs first (RAM-only) and virtio-blk second (persistent, disk-backed); both
stay first-class. The interactive serial console
([[0002-interactive-serial-console]]) already boots a guest to a shell from a RAM
initramfs; what is missing is a place for that shell to write that outlives the
process.

The virtio-mmio device model ([[0003-virtio-mmio-device-model]]) supplies the
substrate: an `MmioBus` that routes `KVM_EXIT_MMIO`, the modern virtio-mmio
transport register file, split virtqueues over `vm-memory` guest RAM, and the
ioeventfd/irqfd fast path. It exposes a **device interface** — device type,
feature bits, config space, and a queue-notify handler — that concrete devices
implement. Because that substrate owns everything from the MMIO exit down to the
used ring and interrupt injection, this design is small: the block-specific config
space, the request grammar, the host-file I/O behind it, and the CLI and image
plumbing to get a real ext4 disk in front of the guest. virtio-blk is the first
consumer of the interface; virtio-net ([[0005-guest-networking-and-ssh]]) will be
the second, reusing it unchanged.

## Detailed Design

The block device is one type — call it `Block` — that implements the substrate's
device trait. Everything below is either that implementation or the host-side
image and kernel story around it. Where a step touches the bus, transport
registers, rings, or the eventfds, it is the substrate's mechanism, named by
reference rather than re-derived.

### 1. Where the block device plugs into the transport

The substrate drives a device through a small interface (modelled on the rust-vmm
`virtio-device` traits). `Block` supplies four things and nothing about the wire
transport:

```text
Block implements the substrate device interface:
  device_type()        -> 2            // virtio 1.2 §5.2.2, VIRTIO_ID_BLOCK
  device_features()    -> VIRTIO_F_VERSION_1 | VIRTIO_BLK_F_FLUSH
  read_config(off,buf) -> serve the virtio_blk_config bytes (capacity)
  activate(mem, queue, interrupt) -> start servicing the notify eventfd
```

- **Device type 2** makes the guest bind `virtio_blk` and expose `/dev/vda`
  (virtio 1.2 §5.2.2); the transport publishes it in its `DeviceID` register.
- **Feature bits.** `Block` advertises `VIRTIO_F_VERSION_1` (bit 32, required by
  the modern transport) and `VIRTIO_BLK_F_FLUSH` (durability, part 4). Everything
  else in §5.2.5 — `RO`, `MQ`, `DISCARD`, geometry, topology — is left off, so the
  negotiated set stays minimal. The transport runs negotiation and the handshake.
- **`activate`** is the hand-off from config plane to data plane: once the guest
  reaches `DRIVER_OK`, the transport calls it with the guest memory handle, the
  ready `Queue`, and an interrupt handle. `Block` registers the queue's notify
  eventfd with the event loop; from then on a guest kick wakes the backend.

`Block` never reads a transport register, parses the rings, or touches the irqfd
ioctl — it calls `queue.iter()` for descriptor chains and `interrupt.signal_used()`
for completions, both provided by the substrate.

### 2. Block config space: capacity

The block config space (virtio 1.2 §5.2.4) is a `struct virtio_blk_config` the
guest reads through the transport's config window. With our minimal feature set
only one field is load-bearing:

```text
struct virtio_blk_config {
    le64 capacity;   // device size in 512-byte sectors   <-- served
    le32 size_max;   // 0 (feature not advertised)
    le32 seg_max;    // 0
    ... geometry, blk_size, topology ...  // all 0, features off
}
```

`capacity = file_len / 512`, computed once from the backing file at construction;
`read_config` answers it little-endian at the field offset, and every other field
reads back zero. A trailing partial sector is truncated so the guest is never told
about a sector we cannot fully back. Capacity is fixed for a run (no online
resize), so config never changes underneath the guest.

### 3. The request format and its descriptor chain

Each request arrives as one descriptor chain (virtio 1.2 §5.2.6) of at least three
segments, handed to us as a substrate `DescriptorChain`:

```text
segment 1  device-readable   struct virtio_blk_req header (16 bytes):
                               le32 type      // IN=0, OUT=1, FLUSH=4
                               le32 reserved
                               le64 sector    // 512-byte LBA
segment 2+ data              device-writable for IN, device-readable for OUT,
                              absent for FLUSH
last       device-writable   u8 status: OK=0, IOERR=1, UNSUPP=2
```

`sector` is always a 512-byte LBA regardless of the host file's block size. The
header and status are fixed book-ends; the data segments between them may be
scattered across guest RAM, so `Block` iterates the chain and reads/writes each
segment via the `vm-memory` handle from `activate`. `virtio-queue` has already
bounds-checked every descriptor against guest RAM, so segment access cannot escape
the guest's own memory; `Block`'s remaining validation is on block semantics —
chain shape, request type, and target LBA.

### 4. Servicing read, write, and flush

`Block` owns the image as a `std::fs::File` opened `O_RDWR`. The block-specific
data-plane loop:

```text
guest kicks QueueNotify -> substrate ioeventfd fires -> event loop wakes Block
Block:
  for chain in queue.iter():
      parse the 16-byte virtio_blk_req header
      match type:
        IN    -> pread  image at sector*512 into device-writable segments
        OUT   -> pwrite device-readable segments to image at sector*512
        FLUSH -> File::sync_all()          // fsync
        else  -> status = UNSUPP
      write the status byte to the last segment
      queue.add_used(head, bytes_written)
  interrupt.signal_used()      // substrate sets InterruptStatus + writes irqfd
```

- **IN** — `read_exact_at(buf, sector * 512)` (a `pread` at an explicit offset, no
  shared cursor) into the data segments; the used-ring length is bytes delivered.
- **OUT** — gather the readable segments and `write_all_at(buf, sector * 512)`
  (`pwrite`); status `OK` on success.
- **FLUSH** — `File::sync_all()` forces the host page cache to stable storage.
  Flush is what makes the success criterion hold: the guest's `sync` / unmount on
  shutdown issues `VIRTIO_BLK_T_FLUSH`, so the bytes reach the host file before
  naos exits — which is why `VIRTIO_BLK_F_FLUSH` is advertised.

Every LBA is range-checked against `capacity` before host I/O. An out-of-range
sector, malformed chain (wrong segment directions, missing status byte), or host
I/O error yields `VIRTIO_BLK_S_IOERR`; an unknown `type` yields
`VIRTIO_BLK_S_UNSUPP`. A request is failed back to the guest, never allowed to
abort the VM. Host I/O is synchronous — the backend blocks in `pread`/`pwrite` on
the I/O thread, fine for a local disk (async deferred — see Open Questions) — and completions go
out through the substrate's irqfd, so a serviced batch costs no userspace MMIO
exits.

### 5. Wiring the drive into the VMM

When `--drive <path>` is present, `Vmm::new` (after the existing IRQ-chip / PIT /
memory / kernel steps in `vmm.rs`) does the block-specific work and hands the rest
to the substrate:

```text
if let Some(path) = drive_path:
    let file = OpenOptions::new().read(true).write(true).open(path)?  // O_RDWR
    let block = Block::new(file)?            // computes capacity from len
    transport.register(block)                // substrate: MMIO window, ioeventfd,
                                             //   irqfd, MmioBus, event-loop sub,
                                             //   virtio_mmio.device= cmdline clause
    cmdline.push("root=/dev/vda rw")         // block-specific: mount the disk
```

The substrate opens the MMIO window, binds the eventfds, inserts the device into
the `MmioBus`, subscribes it to the event loop, and appends the
`virtio_mmio.device=<size>@<addr>:<irq>` discovery clause; this design contributes
only opening the image `O_RDWR` and appending `root=/dev/vda rw`. With no
`--drive`, none of this runs and the initramfs path
([[0002-interactive-serial-console]]) executes byte-for-byte as before.

### 6. Rootfs image and guest kernel

**Image.** A raw file holding a single ext4 filesystem with no partition table, so
the whole disk is the filesystem and the guest mounts `/dev/vda` directly (not
`/dev/vda1`). Built by unpacking a base rootfs into a mounted loopback image:
`truncate` to size, `mkfs.ext4 -F`, mount, extract the rootfs tarball, set up a
serial getty on `ttyS0`, unmount. The base is Alpine's `minirootfs` tarball
(default — tiny, single tarball) or a Debian `debootstrap` tree (heavier, closer
to a real homelab guest). The image lives outside the repo; `DEVELOPMENT.md` / the
`Justfile` grow a build recipe, mirroring how the test vmlinux is produced today.

**Cmdline.** The disk-boot path adds `root=/dev/vda rw` to mount the ext4 root;
`console=ttyS0` and the existing defaults (`reboot=k panic=1 pci=off`) stay, and
the substrate appends the `virtio_mmio.device=` clause. A user-supplied
`--cmdline` is preserved; these clauses are appended, not overridden.

**Guest kernel config deltas.** The disk-boot path needs the block driver and the
filesystem built in (not modules — there is no initramfs to load a module from on
this path):

- `CONFIG_VIRTIO_BLK=y` — the block front-end driver that exposes `/dev/vda`.
- `CONFIG_EXT4_FS=y` — mount the ext4 rootfs.

The virtio core and MMIO transport symbols (`CONFIG_VIRTIO`, `CONFIG_VIRTIO_MMIO`)
are the substrate's kernel requirements ([[0003-virtio-mmio-device-model]]); the
two above are this design's block-specific additions.

## API / Interface Changes

- **New CLI flag** on the `naos-linux` binary:

  ```text
  naos-linux --kernel <PATH> --mem <MIB> [--cmdline <STR>]
             [--initrd <PATH>] [--drive <PATH>]
  ```

  `--drive <PATH>` points at a raw disk image opened `O_RDWR`. It is optional; if
  omitted the VM boots exactly as before (initramfs or bare). `--drive` and an
  initramfs may coexist. (`--rootfs` is a candidate alias — see Open Questions.)
- **Cmdline injection.** When `--drive` is present, naos appends `root=/dev/vda rw`
  alongside the substrate's `virtio_mmio.device=` clause; an explicit `--cmdline`
  is preserved and these clauses are appended.
- **Internal module surface** (new): a `virtio::block` module holding the `Block`
  device — config space, request parsing, host-file I/O — implementing the
  substrate's device interface. It adds no bus or transport surface
  ([[0003-virtio-mmio-device-model]]).

## Data Model

- **On-disk:** a raw image file, no partition table, one ext4 filesystem. naos
  treats it as an opaque array of 512-byte sectors; the ext4 layout is the guest's
  concern. Capacity is `file_len / 512` (virtio 1.2 §5.2.4). This file is the only
  state that persists across runs.
- **In guest memory:** the driver allocates the per-request `virtio_blk_req`
  headers and data buffers (virtio 1.2 §5.2.6), which naos reads or fills; the
  descriptor and ring layout is the substrate's data model. naos allocates no
  guest-side structures.
- **In VMM memory:** `Block` holds the backing `File` and the computed capacity;
  its transport register state, `Queue`, and eventfds belong to the substrate.

## Testing Strategy

Following the house pattern (`DESIGN-naos-linux.md`): unit-test the fiddly logic
and one end-to-end check that proves the whole path.

- **Config-space unit test** (no KVM): construct `Block` over a temp file of known
  length; assert `read_config` returns `len / 512` as a little-endian u64 at the
  capacity offset, and that a non-sector-multiple length truncates.
- **Request-servicing unit tests** (no KVM): back a `GuestMemoryMmap` with
  hand-built descriptor chains and a temp image, drive `Block` directly, and assert
  it parses IN/OUT/FLUSH, writes the correct status byte, lands bytes at
  `sector * 512` in the host file, reports the right used-ring length, and rejects
  malformed chains (`S_IOERR`), unknown types (`S_UNSUPP`), and out-of-range LBAs.
  FLUSH is asserted to call `sync_all`. These reuse the substrate's queue builders.
- **End-to-end acceptance:** `naos-linux --kernel <vmlinux> --drive rootfs.img`
  boots to a serial login; log in, `echo hi > /root/persist`, `sync`, `poweroff`;
  relaunch against the same image; the file is still there. This is the success
  criterion and the single test that proves the whole stack, from `/dev/vda`
  through ext4 to the host file.

KVM-gated and transport-level tests (ioeventfd/irqfd registration, MMIO routing)
belong to the substrate and are not duplicated here.

## Migration / Rollout Plan

Incremental, each step independently observable, so a regression is bisectable to
one commit — the "each milestone de-risks the next" discipline of the ADR ladder
([[0002-microvm-first-incremental-milestone-ladder]]). This picks up after the
substrate lands its bus, transport, and eventfd wiring:

1. **Config-only block device.** Register a `Block` that reports `DeviceID` 2 and
   serves capacity but fails every request. Boot the initramfs kernel with
   `CONFIG_VIRTIO_BLK=y`; confirm the guest binds `virtio_blk` and sees a
   `/dev/vda` of the right size in dmesg, without crashing.
2. **Read path.** Implement IN. Confirm the guest reads the ext4 superblock and
   probes the filesystem from `/dev/vda`.
3. **Write and flush.** Implement OUT and FLUSH. Boot with `--drive` and
   `root=/dev/vda`; reach a serial login on the disk rootfs.
4. **Persistence + polish.** Confirm the write / reboot / persist criterion; add
   the image-build recipe and the `CONFIG_VIRTIO_BLK` / `CONFIG_EXT4_FS` deltas to
   `DEVELOPMENT.md` / the `Justfile`.

The initramfs path is never removed. `--drive` is additive; omitting it yields
byte-for-byte prior behavior, so this cannot regress the serial-console milestone.

## Open Questions

Each item is a decision to settle before this design moves from Draft to Approved.
Option **a** is the recommendation; **b** onward are alternatives; **other** is a
write-in. Record the choice on the **Decision** line.

### 1. Drive flag naming and multi-disk shape

- **a (recommended).** A single `--drive <path>` for now (with `--rootfs` as an
  alias for the boot disk), shaped so it can later become repeatable, each
  occurrence adding a disk in order — matching the single-writable-disk scope.
- **b.** A small spec string (`--drive path=…,readonly=…`) up front for richer
  per-disk options, even though read-only and multi-disk are Non-Goals today.
- **other.** *(write-in)*

**Decision:** *pending*

### 2. In-place reboot versus relaunch

- **a (recommended).** Accept relaunch — a guest reset is a clean VMM exit (prior
  behavior); "reboot" in the success criterion means relaunching naos against the
  same image. Defer a true in-guest reset to a later milestone.
- **b.** Implement a real in-guest reboot now (reset vCPU and device state and
  re-run without exiting).
- **other.** *(write-in)*

**Decision:** *pending*

### 3. Write durability policy

- **a (recommended).** Honor guest `FLUSH` only — advertise `VIRTIO_BLK_F_FLUSH`
  and `fsync` on it; fast and correct for a well-behaved guest that syncs on
  shutdown. Raw image only, no writeback-cache modelling.
- **b.** Also use `O_DIRECT` or a periodic host `fsync` for extra safety against a
  host crash between guest flushes.
- **other.** *(write-in)*

**Decision:** *pending*

### 4. Host I/O engine for network-backed storage

The MVP does synchronous `pread`/`pwrite` on the I/O thread — fine for a local
image, but a network-backed disk (e.g. ZFS-over-iSCSI: the host attaches the
remote zvol as a block device and the backend opens it like any other file) has
high, variable latency, so a slow op stalls the VM's single I/O loop
(head-of-line blocking for its other devices). Neither device model *blocks*
network-backed storage; this is purely about the I/O engine.

- **a (recommended).** Synchronous for the MVP; add an **io_uring** async engine
  (Firecracker's `Async` block engine — submit + eventfd completion, the thread
  never blocks) as the first enhancement once network-backed disks are in use.
  Keeps the single I/O thread and the microVM-density profile.
- **b.** A dedicated per-block-device worker thread (Cloud Hypervisor's model) so
  a slow op blocks only that worker — but adds a thread per disk per VM.
- **other.** *(write-in)*

**Decision:** a — sync for the MVP, io_uring async engine for network-backed storage.

## References

- ADRs: [[0002-microvm-first-incremental-milestone-ladder]],
  [[0005-root-filesystem-initramfs-then-virtio-blk]]
- Substrate and sibling designs: [[0003-virtio-mmio-device-model]] (the transport,
  bus, virtqueues, and irqfd/ioeventfd this builds on),
  [[0001-event-loop-and-concurrency-model]] (backend event loop),
  [[0002-interactive-serial-console]] (initramfs path kept working),
  [[0005-guest-networking-and-ssh]] (reuses the same device interface)
- `DESIGN-naos-linux.md` (MVP scope, house style); `WALK-linux.md` §13 (virtio-blk
  as the first block consumer); current code in `crates/naos-linux/src/`: `vmm.rs`
  (init order), `memory.rs` (guest memory handle)
- Virtio 1.2 spec: §5.2 (block device), §5.2.2 (device ID 2), §5.2.4 (block config
  and `capacity`), §5.2.5 (feature bits), §5.2.6 (`virtio_blk_req` format)
- Guest kernel config (block-specific): `CONFIG_VIRTIO_BLK`, `CONFIG_EXT4_FS`
- rust-vmm crates: `virtio-device` (the device interface `Block` implements),
  `virtio-queue` (`DescriptorChain` iteration), `vm-memory` (guest buffer access)
