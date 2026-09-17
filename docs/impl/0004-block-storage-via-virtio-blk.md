---
id: IMPL-0004
title: "Block storage via virtio-blk"
status: Draft
author: Donald Gifford
created: 2026-07-06
---
<!-- markdownlint-disable-file MD025 MD041 -->

# IMPL 0004: Block storage via virtio-blk

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
  - [Phase 1: Block device, config space, and the drive flags](#phase-1-block-device-config-space-and-the-drive-flags)
    - [Tasks](#tasks)
    - [Success Criteria](#success-criteria)
  - [Phase 2: Servicing requests with a synchronous engine](#phase-2-servicing-requests-with-a-synchronous-engine)
    - [Tasks](#tasks-1)
    - [Success Criteria](#success-criteria-1)
  - [Phase 3: The asynchronous block engine](#phase-3-the-asynchronous-block-engine)
    - [Tasks](#tasks-2)
    - [Success Criteria](#success-criteria-2)
  - [Phase 4: Booting from a virtio-blk root filesystem](#phase-4-booting-from-a-virtio-blk-root-filesystem)
    - [Tasks](#tasks-3)
    - [Success Criteria](#success-criteria-3)
  - [Phase 5: Testing and durability](#phase-5-testing-and-durability)
    - [Tasks](#tasks-4)
    - [Success Criteria](#success-criteria-4)
- [Open Questions](#open-questions)
  - [1. Block engine crate and completion signalling](#1-block-engine-crate-and-completion-signalling)
  - [2. Shipping the synchronous engine as a fallback](#2-shipping-the-synchronous-engine-as-a-fallback)
  - [3. Direct cache mode alignment with guest buffers](#3-direct-cache-mode-alignment-with-guest-buffers)
  - [4. Disk image format for tests](#4-disk-image-format-for-tests)
- [File Changes](#file-changes)
- [Testing Plan](#testing-plan)
- [References](#references)
<!--toc:end-->

## Objective

Give naos a persistent, disk-backed root filesystem by implementing a
**virtio-blk** device (virtio 1.2 §5.2) as the first consumer of the
virtio-mmio substrate. The device serves a block config space (capacity),
parses `virtio_blk_req` chains off one split virtqueue, and services read,
write, and flush requests against a raw host image, first with a synchronous
file engine and then with an `io_uring` asynchronous engine driven by the
event loop. The guest boots from `/dev/vda` to a serial login and its writes
survive across runs.

**Implements:** [[0004-block-storage-via-virtio-blk]]

## Scope

### In Scope

- A `Block` device implementing the substrate's `VirtioDevice` trait: device
  type 2 (`VIRTIO_ID_BLOCK`), feature bits `VIRTIO_F_VERSION_1` and
  `VIRTIO_BLK_F_FLUSH`, and a `virtio_blk_config` config space serving capacity.
- Request parsing (virtio 1.2 §5.2.6): the 16-byte `virtio_blk_req` header,
  scattered data segments, and the trailing status byte, over the substrate's
  `DescriptorChain` iteration.
- Servicing `VIRTIO_BLK_T_IN`, `VIRTIO_BLK_T_OUT`, and `VIRTIO_BLK_T_FLUSH`
  against a raw image file opened `O_RDWR`.
- Two host I/O engines behind one interface: a synchronous `pread`/`pwrite`
  engine and an `io_uring` asynchronous engine whose completions are serviced on
  the event loop.
- A per-disk cache mode (buffered by default, `O_DIRECT` opt-in) and honoring
  guest `FLUSH` via `fsync`; no periodic host `fsync`.
- The `--drive <path>` CLI flag with a `--rootfs` alias, cmdline injection of
  `root=/dev/vda rw`, and an image-build recipe.
- Unit tests for config space and request parsing, plus a data-integrity and
  durability end-to-end test.

### Out of Scope

- The MMIO bus, virtio-mmio transport register file, split-virtqueue parsing,
  feature-negotiation windows, status handshake, ioeventfd doorbell, and irqfd
  injection — all owned by [[0003-virtio-mmio-device-model]].
- The epoll event loop, irqfd/ioeventfd primitives, and threading model, owned
  by [[0001-event-loop-and-concurrency-model]].
- Multiple queues (`VIRTIO_BLK_F_MQ`), multiple disks, hotplug, online resize,
  read-only images, and discard/TRIM.
- Rich image formats (qcow2); raw images only.
- In-place guest reboot; reboot means relaunching naos against the same image.

## Current State

The crate boots a `vmlinux` ELF to a serial console and has no virtio and no
block device. `main.rs` parses exactly three flags (`--kernel`, `--mem`,
`--cmdline`) into `Args` and calls `Vmm::new`; there is no drive or rootfs flag.
`Vmm::new` in `vmm.rs` runs a fixed init order — open `/dev/kvm`, create the VM,
set the TSS address, create the in-kernel IRQ chip and PIT, build and register
guest memory, load the kernel, write the cmdline and `boot_params`, create the
serial device, then create and configure the vCPU. The cmdline is passed through
verbatim from `Args::cmdline` (default `console=ttyS0 reboot=k panic=1 pci=off`)
by `boot::write_cmdline` and `boot::write_boot_params`; nothing appends to it.
`vcpu::run` handles PIO, `Hlt`, `Shutdown`, and a reset request and treats any
other exit (including MMIO) as an error.

This IMPL adds the first real virtio device on top of the virtio-mmio substrate
delivered by [[0003-virtio-mmio-device-model]] (the `MmioBus`, the transport
register model, the `VirtioDevice` trait, `Interrupt`, and the `virtio-queue`
glue). The block device plugs into that transport and never touches a transport
register, the rings, or the irqfd ioctl directly.

## Dependencies

- **[[0003-virtio-mmio-device-model]]** — hard dependency. This device
  implements that substrate's `VirtioDevice` trait (identity, feature bits,
  config space, `activate`, `process_queue`) and is registered into the
  `MmioBus`. It relies on the transport for feature negotiation, the status
  handshake, the QueueNotify ioeventfd, `DescriptorChain` iteration, and the
  `Interrupt` used-ring signal. Without the substrate there is no transport for
  the device to attach to.
- **[[0001-event-loop-and-concurrency-model]]** — hard dependency. The data
  plane runs on the epoll event loop (`event-manager` `EventManager` /
  `Subscriber`): a guest kick wakes `process_queue` through the substrate's
  ioeventfd, and the `io_uring` engine's completion eventfd is a second event-loop
  subscriber. Completions are injected through the substrate's irqfd, which the
  event loop wires. Without the event loop the async engine has nowhere to be
  driven from.
- **New crate: `io-uring`** (the tokio-rs `io-uring` crate, 0.7.x) — the async
  block engine's submission/completion queues and eventfd completion signal (see
  Open Questions for the exact version pin and integration).
- **Reused from IMPL-0003:** `virtio-queue` (`Queue`, `DescriptorChain`) and
  `virtio-bindings` (`virtio_blk` constants and `virtio_blk_config`). These are
  added by the substrate; this device consumes them.
- **Guest kernel:** `CONFIG_VIRTIO_BLK=y` (the block front-end that exposes
  `/dev/vda`) and `CONFIG_EXT4_FS=y` (to mount the rootfs), on top of the
  substrate's `CONFIG_VIRTIO` and `CONFIG_VIRTIO_MMIO`.

## Implementation Phases

Each phase keeps the build green (`cargo build` and `cargo test` succeed,
`cargo clippy` clean) and is independently observable, so a regression is
bisectable to one commit. Phases 1 through 3 harden the device against unit
tests without KVM; phase 4 is the first end-to-end boot; phase 5 proves
integrity and durability.

### Phase 1: Block device, config space, and the drive flags

Stand up a `Block` device that implements `VirtioDevice`, reports its identity
and features, serves capacity from the backing file, and can be selected from
the CLI — but fails every request. This mirrors the design's "config-only" first
rollout step.

#### Tasks

- [ ] Add a `virtio::block` module (`src/virtio/block.rs`) defining `Block`,
      owning the backing `std::fs::File` and the computed capacity.
- [ ] Implement `device_type` returning `VIRTIO_ID_BLOCK` (2), and
      `device_features`/`ack_features` advertising `VIRTIO_F_VERSION_1` plus
      `VIRTIO_BLK_F_FLUSH` (from `virtio-bindings`).
- [ ] Implement `read_config`/`write_config` over `virtio_blk_config`: serve
      `capacity = file_len / 512` little-endian at the capacity offset, truncate
      a trailing partial sector, and read every other field back as zero.
- [ ] Implement `queue_max_sizes` returning a single queue, and stub `activate`
      and `process_queue` (store the queue/memory/interrupt; complete nothing).
- [ ] Add `--drive <PATH>` with a `--rootfs` alias to `Args` in `main.rs`
      (optional; `clap` `visible_alias`), shaped so it can later grow into a
      repeatable spec string; thread the path into `Vmm::new`.
- [ ] In `vmm.rs`, when a drive is present, open the image with
      `OpenOptions::new().read(true).write(true)` (`O_RDWR`), construct `Block`,
      and register it with the substrate transport.
- [ ] Write unit tests for the config space (capacity from a temp file of known
      length, partial-sector truncation, zero for other fields) and for the CLI
      (`--drive` and `--rootfs` parse to the same field; both optional).

#### Success Criteria

- With `--drive`, a guest kernel built `CONFIG_VIRTIO_BLK=y` binds `virtio_blk`
  and shows a `/dev/vda` of the right size in `dmesg` without crashing.
- Without `--drive`, the boot path is byte-for-byte the prior behavior.
- Config-space and CLI unit tests pass with no KVM.

### Phase 2: Servicing requests with a synchronous engine

Parse `virtio_blk_req` chains and service IN, OUT, and FLUSH against the image
with a straightforward file-backed synchronous engine, then complete each
request on the used ring and raise the interrupt. This is the readable
reference implementation the async engine is validated against.

#### Tasks

- [ ] Add a `virtio::block::request` submodule that parses one `DescriptorChain`
      into a typed request: 16-byte header (`type`, reserved, `sector`), the
      device-readable or device-writable data segments, and the trailing
      device-writable status byte; reject malformed chains.
- [ ] Define a `BlockEngine` interface (submit an IN/OUT/FLUSH against the file,
      report completion) so the engine is swappable in phase 3.
- [ ] Implement `SyncEngine`: IN via `read_exact_at(buf, sector * 512)`, OUT via
      `write_all_at(buf, sector * 512)` (`FileExt` positioned I/O), FLUSH via
      `File::sync_all`; range-check every LBA against capacity first.
- [ ] Implement `process_queue`: iterate `queue.iter()`, service each chain,
      write the status byte (`VIRTIO_BLK_S_OK` / `S_IOERR` / `S_UNSUPP`),
      `add_used(head, bytes_written)`, then `interrupt.signal()` once per batch.
- [ ] Ensure a bad request is failed back to the guest, never allowed to abort
      the VM: out-of-range LBA or host I/O error yields `S_IOERR`, an unknown
      `type` yields `S_UNSUPP`.
- [ ] Write request-parsing unit tests over a hand-built `GuestMemoryMmap` and a
      temp image: assert IN/OUT/FLUSH land bytes at `sector * 512`, report the
      right used-ring length, set the right status byte, and reject malformed
      chains, unknown types, and out-of-range LBAs; assert FLUSH calls `sync_all`.

#### Success Criteria

- The guest reads the ext4 superblock and probes the filesystem from `/dev/vda`.
- Writes issued by the guest appear at the correct byte offset in the host image.
- Request-parsing unit tests pass with no KVM.

### Phase 3: The asynchronous block engine

Add an `io_uring`-backed engine behind the same `BlockEngine` interface, driven
by the event loop so a slow op never stalls the I/O thread. This is the design's
planned enhancement for high-latency, network-backed storage.

#### Tasks

- [ ] Add the `io-uring` dependency and implement `UringEngine`: build an
      `IoUring`, and translate each request into an `opcode::Read` /
      `opcode::Write` / `opcode::Fsync` submission-queue entry, encoding the
      request head index in the SQE `user_data`.
- [ ] Register a completion `EventFd` on the ring
      (`Submitter::register_eventfd`) and subscribe it to the event loop as a
      second `Subscriber`, so a batch of completions wakes the device (integration
      detail in Open Questions).
- [ ] On the completion wake, drain the completion queue, map each CQE's
      `user_data` back to its pending chain, write the status byte, `add_used`,
      and `interrupt.signal()`.
- [ ] Add a per-disk cache mode: buffered by default, `O_DIRECT`
      (`OpenOptions` `custom_flags(O_DIRECT)`) as an opt-in for block devices and
      network-backed storage; keep honoring guest FLUSH and do NOT add a periodic
      host `fsync`.
- [ ] Wire the engine choice through the drive configuration (default async;
      keep the synchronous engine selectable per Open Questions).
- [ ] Write unit tests exercising the async submit/complete path against a temp
      image where KVM is not required (drive the engine directly, poll the
      completion eventfd), asserting the same data-integrity invariants as the
      synchronous engine.

#### Success Criteria

- IN/OUT/FLUSH complete correctly through the async engine with the same
  data-integrity results as `SyncEngine`.
- The I/O thread never blocks in a host read/write; completions arrive via the
  eventfd subscriber.
- FLUSH still forces data to stable storage; no periodic host `fsync` runs.

### Phase 4: Booting from a virtio-blk root filesystem

Boot a real rootfs image from the block device end to end: build a raw ext4
image, inject `root=/dev/vda rw`, and reach a serial login on the disk rootfs.

#### Tasks

- [ ] In `vmm.rs`, when a drive is present, append `root=/dev/vda rw` to the
      cmdline alongside the substrate's `virtio_mmio.device=<size>@<addr>:<irq>`
      clause; preserve an explicit `--cmdline` (append, do not override).
- [ ] Add an image-build recipe to the `Justfile` and `DEVELOPMENT.md`:
      `truncate` to size, `mkfs.ext4 -F`, mount, unpack an Alpine `minirootfs`
      tarball, set up a serial getty on `ttyS0`, unmount.
- [ ] Document the guest kernel deltas (`CONFIG_VIRTIO_BLK=y`,
      `CONFIG_EXT4_FS=y`) next to the existing kernel-build instructions.
- [ ] Confirm the initramfs path ([[0002-interactive-serial-console]]) still
      works and that `--drive` and an initramfs may coexist.
- [ ] Manually verify a boot: `naos-linux --kernel <vmlinux> --drive rootfs.img`
      reaches a serial login backed by the ext4 rootfs.

#### Success Criteria

- The guest boots from `/dev/vda` through ext4 to a serial login shell.
- Omitting `--drive` yields the prior initramfs or bare-boot behavior unchanged.

### Phase 5: Testing and durability

Lock in the request-parsing unit coverage, prove data integrity through a real
boot, and prove that a guest flush makes writes survive a relaunch.

#### Tasks

- [ ] Consolidate request-parsing and config-space unit tests (from phases 1
      and 2), including malformed-chain, unknown-type, and out-of-range cases.
- [ ] Add a data-integrity end-to-end test (`tests/block_e2e.rs`, KVM-gated,
      skips cleanly without `/dev/kvm`): boot with a raw image, write and read
      back a known pattern from the guest, and assert it round-trips.
- [ ] Add a durability end-to-end test: boot, `echo hi > /root/persist`, `sync`,
      `poweroff`; relaunch against the same image; assert the file persists — the
      design's success criterion.
- [ ] Assert at the host level that the bytes are present in the raw image file
      after the guest's FLUSH, exercising `VIRTIO_BLK_F_FLUSH` end to end.
- [ ] Confirm `cargo test`, `cargo clippy`, and `cargo fmt --check` are clean and
      the e2e tests skip without KVM or the image artifact.

#### Success Criteria

- Unit tests cover IN/OUT/FLUSH parsing, status codes, and rejection paths.
- The data-integrity and durability e2e tests pass on a KVM host and skip
  cleanly without one.

## Open Questions

Implementation-level decisions to settle as the code lands. Option **a** is the
recommendation; **b** onward are alternatives; **other** is a write-in. Record
the choice on the **Decision** line. Device-level policy (drive-flag naming,
reboot-by-relaunch, FLUSH-only durability, the async engine choice) is already
decided in [[0004-block-storage-via-virtio-blk]] and is not re-opened here.

### 1. Block engine crate and completion signalling

The async engine needs a submission/completion interface and a way to wake the
event loop on completion. The `io-uring` crate's ring fd is epoll-readable, but a
registered eventfd is the more common integration and matches Firecracker's
`Async` engine.

- **a** (recommended) — Use the tokio-rs `io-uring` crate pinned to a current
  `0.7.x`, register a completion `EventFd` via `Submitter::register_eventfd`, and
  subscribe that eventfd to the `event-manager` loop. Standard, decoupled from the
  ring internals, and reuses the substrate's `EventFd`/`Subscriber` machinery.
- **b** — Register the ring fd itself with epoll (readable when the completion
  queue is non-empty), avoiding the extra eventfd. Fewer fds, but a less common
  and more finicky readiness contract.
- **other** — *(write-in)*

**Decision:** a — use the tokio-rs `io-uring` crate pinned to a current `0.7.x`, register a completion `EventFd` via `Submitter::register_eventfd`, and subscribe it to the event loop.

### 2. Shipping the synchronous engine as a fallback

Phase 2 builds a synchronous engine; phase 3 adds the async one. Question is
whether the synchronous engine ships as a supported fallback or is only a
validation stepping stone.

- **a** (recommended) — Keep the synchronous engine behind the `BlockEngine`
  interface as a supported, selectable fallback (default async). It is the
  readable reference the async path is tested against, and it is the safe choice
  on hosts with an old kernel or restricted `io_uring`.
- **b** — Go straight to `io_uring` and delete the synchronous engine once the
  async path works, minimizing surface area.
- **other** — *(write-in)*

**Decision:** a — keep the synchronous engine behind the `BlockEngine` interface as a supported, selectable fallback (default async); it is the reference the async path is tested against and the safe choice on restricted hosts.

### 3. Direct cache mode alignment with guest buffers

`O_DIRECT` requires the host buffer address, length, and file offset to be
aligned to the logical block size. Guest data segments come straight from guest
RAM at arbitrary offsets, so zero-copy submission may violate alignment.

- **a** (recommended) — For a misaligned segment under `O_DIRECT`, bounce through
  an aligned host buffer (aligned allocation, copy in/out); use zero-copy directly
  when the guest segment already satisfies alignment. Correct for any guest, at a
  copy cost only on the misaligned path. Buffered mode (the default) has no such
  constraint.
- **b** — Require aligned guest buffers and fail an unaligned request with
  `S_IOERR`, pushing alignment onto the guest driver. Simpler host code, but
  fragile against real drivers.
- **other** — *(write-in)*

**Decision:** a — bounce a misaligned segment through an aligned host buffer under `O_DIRECT`, and submit zero-copy when the guest segment already satisfies alignment.

### 4. Disk image format for tests

- **a** (recommended) — Raw images only, matching the design's raw-only scope: a
  small `mkfs.ext4` raw file for e2e, and temp raw files for unit tests. No format
  layer to model.
- **b** — Add a tiny synthetic format or sparse-file handling for tests, closer to
  a production image pipeline but out of scope for this milestone.
- **other** — *(write-in)*

**Decision:** a — raw images only: a small `mkfs.ext4` raw file for e2e and temp raw files for unit tests, with no format layer.

## File Changes

| File | Action | Description |
|------|--------|-------------|
| `crates/naos-linux/src/virtio/block.rs` | Create | The `Block` device: `VirtioDevice` impl, `virtio_blk_config` config space, capacity, `activate`/`process_queue`. |
| `crates/naos-linux/src/virtio/block/request.rs` | Create | Parse one `DescriptorChain` into a typed `virtio_blk_req` (header, data segments, status byte); reject malformed chains. |
| `crates/naos-linux/src/virtio/block/engine.rs` | Create | The `BlockEngine` interface plus `SyncEngine` (`pread`/`pwrite`/`fsync`) and `UringEngine` (`io_uring` submit + eventfd completion). |
| `crates/naos-linux/src/main.rs` | Modify | Add `--drive <PATH>` with a `--rootfs` alias to `Args`; thread the path into `Vmm::new`. |
| `crates/naos-linux/src/vmm.rs` | Modify | Open the image `O_RDWR`, construct and register `Block` with the transport, append `root=/dev/vda rw` when a drive is present. |
| `crates/naos-linux/Cargo.toml` | Modify | Add `io-uring`; consume `virtio-queue` / `virtio-bindings` introduced by IMPL-0003. |
| `crates/naos-linux/tests/block_e2e.rs` | Create | KVM-gated data-integrity and durability end-to-end tests; skip without `/dev/kvm` or the image artifact. |
| `Justfile` | Modify | Image-build recipe (`truncate`, `mkfs.ext4 -F`, unpack rootfs, serial getty). |
| `DEVELOPMENT.md` | Modify | Document the image recipe and the `CONFIG_VIRTIO_BLK` / `CONFIG_EXT4_FS` kernel deltas. |

## Testing Plan

- [ ] Config-space unit tests: capacity from a temp file of known length,
      partial-sector truncation, zero for unadvertised fields (no KVM).
- [ ] CLI unit tests: `--drive` and `--rootfs` parse to the same optional field
      (no KVM).
- [ ] Request-parsing unit tests: IN/OUT/FLUSH over hand-built descriptor chains
      and a temp image — correct byte offset, used-ring length, status byte, and
      rejection of malformed chains, unknown types, and out-of-range LBAs;
      FLUSH calls `sync_all` (no KVM).
- [ ] Async-engine unit tests: submit/complete over the completion eventfd
      against a temp image, matching the synchronous data-integrity invariants
      (no KVM).
- [ ] Data-integrity e2e (**KVM-gated**): boot from a raw image, write and read
      back a known pattern from the guest, assert it round-trips.
- [ ] Durability e2e (**KVM-gated**): write, `sync`, `poweroff`, relaunch against
      the same image, assert the file persists; confirm the bytes are present in
      the host image file after FLUSH.
- [ ] Regression: with no `--drive`, the existing `boot_e2e` behavior is unchanged
      (**KVM-gated**).

## References

- [[0004-block-storage-via-virtio-blk]] — source design
- [[0003-virtio-mmio-device-model]] — the transport, `MmioBus`, `VirtioDevice`
  trait, `Interrupt`, and `virtio-queue` glue this device plugs into
- [[0001-event-loop-and-concurrency-model]] — the epoll event loop, `EventFd`,
  irqfd/ioeventfd the data plane runs on
- [[0002-interactive-serial-console]] — the initramfs path kept working
- ADRs: [[0002-microvm-first-incremental-milestone-ladder]],
  [[0004-virtio-over-mmio-device-transport]],
  [[0005-root-filesystem-initramfs-then-virtio-blk]]
- Virtio 1.2 spec: §5.2 (block device), §5.2.2 (device ID 2), §5.2.4 (config and
  `capacity`), §5.2.5 (feature bits), §5.2.6 (`virtio_blk_req` format)
- Crate docs: `io-uring` (async engine), `virtio-queue` (`DescriptorChain`),
  `virtio-bindings` (`virtio_blk` constants), `vm-memory` (guest buffer access)
- Current code: `crates/naos-linux/src/main.rs` (CLI), `vmm.rs` (init order and
  cmdline assembly), `boot.rs` (`write_cmdline` / `write_boot_params`),
  `tests/boot_e2e.rs` (e2e harness pattern)
- Guest kernel config: `CONFIG_VIRTIO_BLK`, `CONFIG_EXT4_FS`
