---
id: IMPL-0003
title: "virtio-mmio device model"
status: Draft
author: Donald Gifford
created: 2026-07-06
---
<!-- markdownlint-disable-file MD025 MD041 -->

# IMPL 0003: virtio-mmio device model

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
  - [Phase 1: MMIO bus and transport register file](#phase-1-mmio-bus-and-transport-register-file)
    - [Tasks](#tasks)
    - [Success Criteria](#success-criteria)
  - [Phase 2: Split virtqueue integration](#phase-2-split-virtqueue-integration)
    - [Tasks](#tasks-1)
    - [Success Criteria](#success-criteria-1)
  - [Phase 3: Device trait and interrupt plumbing](#phase-3-device-trait-and-interrupt-plumbing)
    - [Tasks](#tasks-2)
    - [Success Criteria](#success-criteria-2)
  - [Phase 4: Guest discovery and slot allocation](#phase-4-guest-discovery-and-slot-allocation)
    - [Tasks](#tasks-3)
    - [Success Criteria](#success-criteria-3)
  - [Phase 5: Null test device and end-to-end wiring](#phase-5-null-test-device-and-end-to-end-wiring)
    - [Tasks](#tasks-4)
    - [Success Criteria](#success-criteria-4)
- [Open Questions](#open-questions)
  - [1. Confirm the MMIO window base, stride, and GSI against the memory map](#1-confirm-the-mmio-window-base-stride-and-gsi-against-the-memory-map)
  - [2. Whether to depend on the unpublished virtio-device crate or define the trait in-crate](#2-whether-to-depend-on-the-unpublished-virtio-device-crate-or-define-the-trait-in-crate)
  - [3. How the vCPU-thread MMIO dispatch coordinates with the coarse device mutex](#3-how-the-vcpu-thread-mmio-dispatch-coordinates-with-the-coarse-device-mutex)
  - [4. Whether guest memory moves from GuestMemoryMmap to GuestMemoryAtomic](#4-whether-guest-memory-moves-from-guestmemorymmap-to-guestmemoryatomic)
  - [5. What the minimal test device should be](#5-what-the-minimal-test-device-should-be)
- [File Changes](#file-changes)
- [Testing Plan](#testing-plan)
- [References](#references)
<!--toc:end-->

## Objective

Build the device-agnostic virtio substrate: an MMIO bus that routes guest MMIO
vmexits to devices, a modern virtio-mmio transport register model, split
virtqueues over `vm-memory`, a `VirtioDevice` trait, and the ioeventfd/irqfd
data-plane plumbing — exercised end to end by a null test device. This ships the
foundation that block ([[0004-block-storage-via-virtio-blk]]) and net
([[0005-guest-networking-and-ssh]]) plug into; it carries no device logic itself.

**Implements:** [[0003-virtio-mmio-device-model]]

## Scope

### In Scope

- An `MmioBus` that maps `[base, base+len)` guest-physical ranges to device
  handles and dispatches `MmioRead`/`MmioWrite` from `vcpu::run`, replacing the
  defensive `bail!` arm for MMIO exits.
- The modern (virtio 1.2 §4.2.2) virtio-mmio transport register file for one
  device per fixed slot: magic/version/device-id, 64-bit windowed feature
  negotiation, queue configuration registers, and the device-status handshake.
- Split-virtqueue integration via the rust-vmm `virtio-queue` crate over guest
  memory (descriptor table, available ring, used ring).
- A `VirtioDevice` trait plus an `Interrupt` handle, with a coarse
  `Arc<Mutex<dyn VirtioDevice>>` (config plus queue state) and an
  `Arc<AtomicU32>` `InterruptStatus` (Firecracker's model).
- Queue-notify `ioeventfd` and used-ring `irqfd` registered against the `VmFd`
  and serviced by the event-loop thread from [[0001-event-loop-and-concurrency-model]].
- The `virtio_mmio.device=<size>@<addr>:<irq>` cmdline token generator and a
  fixed device-slot array (base, stride, GSI per slot).
- A null/loopback test device and the unit, virtqueue-round-trip, and
  integration tests that drive the transport.

### Out of Scope

- Any concrete device. virtio-blk is [[0004-block-storage-via-virtio-blk]];
  virtio-net is [[0005-guest-networking-and-ssh]]. This IMPL stops at the trait.
- virtio-pci or legacy virtio; multiple queues per device, indirect descriptors,
  or packed virtqueues; device hotplug or a dynamic MMIO/GSI allocator
  (`vm-allocator` is deferred, per design Q4).
- The event loop, ioeventfd/irqfd delivery path, and vCPU/IO threading
  themselves — owned by [[0001-event-loop-and-concurrency-model]]. This work
  subscribes to that machinery, it does not build it.
- CLI flags that instantiate a device (for example a block `--drive`); those
  belong to the consumer IMPLs.

## Current State

There is no virtio anywhere in the crate today. The relevant surfaces this IMPL
must extend:

- **MMIO exit handling.** `vcpu::run` (`crates/naos-linux/src/vcpu.rs`) handles
  only `IoOut`/`IoIn` (PIO), `Hlt`, `Shutdown`, and a reset request; any MMIO
  exit falls through to the defensive `bail!("Unexpected vCPU exit")` arm. The
  `run_errors_on_an_unhandled_exit` test deliberately triggers that arm by
  reading `0x200000` (mapped by the boot page tables but backed by no memory
  slot). Adding `MmioRead`/`MmioWrite` arms means that test must move to a
  still-unmapped address.
- **Guest memory map.** `memory::build` (`memory.rs`) allocates one contiguous
  `GuestMemoryMmap` region from guest-physical `0`, sized by `--mem` MiB
  (default 256), with no MMIO hole. `boot.rs` identity-maps only the first 1 GiB
  (one PML4, one PDPT, one PD of 2 MiB pages) and sets the TSS at `0xFFFB_D000`
  (near 4 GiB) in `vmm.rs`. Nothing today reserves the proposed virtio window at
  `0xd0000000`, and nothing bounds `--mem` away from it.
- **Command line assembly.** `main.rs` defaults the cmdline to
  `console=ttyS0 reboot=k panic=1 pci=off`; `vmm::Vmm::new` passes it straight to
  `boot::write_cmdline` / `boot::write_boot_params`. There is no place today that
  appends VMM-generated tokens to a user cmdline.
- **IRQ infrastructure.** `vmm::Vmm::new` already calls `create_irq_chip`
  (IOAPIC plus PIC) and `create_pit2` before vCPU creation, so GSI routing
  exists; only the `register_irqfd` binding per device is new.
- **Threading.** `vcpu::run` is still the synchronous single-thread blocking
  `KVM_RUN` loop; the split into a vCPU thread and an event-loop thread is
  delivered by [[0001-event-loop-and-concurrency-model]] (see Dependencies).

## Dependencies

- **Hard dependency on [[0001-event-loop-and-concurrency-model]].** The data
  plane requires the event-loop thread and the two eventfds it wires: a
  queue-notify `ioeventfd` (guest writes to the QueueNotify register fire an
  `EventFd` inside KVM with a datamatch on the queue index — no MMIO vmexit, no
  `MmioBus` dispatch) and a used-ring `irqfd` (the backend writes an `EventFd`
  bound to the device GSI and KVM injects the interrupt through the in-kernel
  IOAPIC). Both are created, registered, and serviced by the event loop. By
  contrast, the config-plane transport dispatch (probe, feature negotiation,
  status, reset) runs synchronously on the **vCPU thread** as the `MmioBus`
  handler of an `MmioRead`/`MmioWrite` vmexit. This IMPL therefore lands after,
  or is co-sequenced with, IMPL-0001; the coarse device state is shared across
  those two threads behind the device mutex, with `InterruptStatus` atomic.
- **New crates (rust-vmm).**
  - `virtio-queue` 0.17.0 — `Queue`/`QueueSync`, `QueueT`/`QueueOwnedT`,
    `DescriptorChain`, `Reader`/`Writer`. It depends on `vm-memory ^0.17.1`,
    which matches the crate's existing `vm-memory` 0.17.1 pin, so its
    `GuestMemory` bounds compose with our `GuestMemoryMmap` (confirm exact
    generic bounds while coding — see Open Questions).
  - `virtio-bindings` 0.2.7 — generated FFI constants: `VIRTIO_F_VERSION_1`,
    device-status bits, `VIRTQ_DESC_F_NEXT`/`VIRTQ_DESC_F_WRITE`, device-type ids.
  - The `virtio-device` trait crate named in the design is **not published to
    crates.io** (only `virtio-devices` plural and `dbs-virtio-devices` exist), so
    the `VirtioDevice` trait is defined in-crate rather than imported — see Open
    Questions.
- **Guest kernel.** The guest must be built with `CONFIG_VIRTIO=y` and
  `CONFIG_VIRTIO_MMIO=y` so `drivers/virtio/virtio_mmio.c` parses the
  `virtio_mmio.device=` token and probes the window. Device-class symbols
  (`CONFIG_VIRTIO_BLK`, `CONFIG_VIRTIO_NET`) belong to the consumer IMPLs.

## Implementation Phases

Each phase keeps the build green. With no device configured, the boot path is
byte-for-byte identical to today, so no phase can regress the milestones beneath
it.

### Phase 1: MMIO bus and transport register file

Stand up the address-routing bus and a virtio-mmio transport register file for
one device at a fixed slot, behind a stub device, and route MMIO vmexits to it.
No queues or interrupts yet.

#### Tasks

- [ ] Add `src/mmio.rs` with an `MmioBus` that owns an ordered map of
      `[base, base+len)` ranges to handles, resolves `addr - base` to a register
      offset, and returns zeroes / ignores writes for an unmapped address (the
      PIO "return 0xFF for unknown ports" convention).
- [ ] Add `src/virtio/transport.rs` implementing the modern virtio-mmio register
      layout (virtio 1.2 §4.2.2): MagicValue `0x74726976`, Version `2`, DeviceID,
      VendorID, windowed DeviceFeatures/DriverFeatures via the `*Sel` registers,
      and the `Status` handshake byte, delegating identity/features to a device.
- [ ] Replace the `bail!` MMIO fall-through in `vcpu::run` with `MmioRead`/
      `MmioWrite` arms that call `bus.read(addr, data)` / `bus.write(addr, data)`;
      thread a shared `MmioBus` handle into `vcpu::run` and `Vmm`.
- [ ] Update the `run_errors_on_an_unhandled_exit` test to fault a still-unmapped
      exit reason (a different unhandled `VcpuExit`, or an address the bus does
      not back) now that MMIO no longer bails.
- [ ] Write unit tests: adjacent range dispatch, unmapped-address no-op, offset
      translation at range edges; MagicValue/Version/DeviceID read back behind a
      mock device; the `Status` sequence advances only through
      `ACKNOWLEDGE` to `DRIVER` to `FEATURES_OK` to `DRIVER_OK`.

#### Success Criteria

- `cargo build` and `cargo clippy` are clean; the existing suite still passes.
- MMIO reads/writes route to the registered device and unmapped MMIO is a no-op,
  proven by unit tests with no `/dev/kvm` requirement.

### Phase 2: Split virtqueue integration

Wire `virtio-queue` split virtqueues over guest memory and read the queue
configuration the guest programs through the transport registers.

#### Tasks

- [ ] Add `virtio-queue` 0.17.0 and `virtio-bindings` 0.2.7 to `Cargo.toml`,
      each with a one-line rationale comment matching the crate's house style.
- [ ] In the transport, decode QueueSel/QueueNumMax/QueueNum/QueueReady and the
      `Queue{Desc,Driver,Device}{Low,High}` register pairs into the `QueueT`
      setters (`set_size`, `set_desc_table_address`, `set_avail_ring_address`,
      `set_used_ring_address`, `set_ready`), assembling the 64-bit addresses from
      the low/high halves.
- [ ] Hold one `Queue` per device slot (single split virtqueue for the
      substrate) and confirm it composes with `GuestMemoryMmap` at
      `vm-memory` 0.17.1.
- [ ] Add a helper that walks a `DescriptorChain`, honoring `VIRTQ_DESC_F_NEXT`
      and `VIRTQ_DESC_F_WRITE`, and publishes `(head, len)` to the used ring via
      `add_used`.
- [ ] Write unit tests: back a `GuestMemoryMmap` with hand-built descriptor
      chains and assert `NEXT`/`WRITE` flags are honored, the right `(head, len)`
      lands in the used ring, and a malformed chain is rejected — no `/dev/kvm`.

#### Success Criteria

- The transport reflects guest-programmed queue addresses/size/ready into a live
  `Queue`.
- A hand-built descriptor chain round-trips through the walk-and-`add_used`
  helper in a unit test.

### Phase 3: Device trait and interrupt plumbing

Define the `VirtioDevice` trait and the `Interrupt` handle, wrap the transport
plus device as a coarse `Arc<Mutex<dyn VirtioDevice>>` with an atomic
`InterruptStatus`, and register the two eventfds with the event loop.

#### Tasks

- [ ] Add `src/virtio/device.rs` with the `VirtioDevice` trait (`device_type`,
      `device_features`, `ack_features`, `queue_max_sizes`, `read_config`,
      `write_config`, `activate`, `process_queue`) — defined in-crate, since the
      rust-vmm `virtio-device` crate is unpublished (see Open Questions).
- [ ] Add an `Interrupt` wrapping the device's irqfd `EventFd` plus the shared
      `Arc<AtomicU32>` `InterruptStatus`; `signal()` sets the used-buffer bit and
      writes the eventfd.
- [ ] Drive `activate` exactly once on the `DRIVER_OK` status transition, handing
      the device its `Queue`(s), guest memory, and `Interrupt`; reset on a `0`
      write to `Status`.
- [ ] Register the QueueNotify `ioeventfd` (`VmFd::register_ioevent`, datamatch on
      queue index at the device's `0x050` register) and the GSI `irqfd`
      (`VmFd::register_irqfd`) and subscribe `process_queue` to the event loop
      from [[0001-event-loop-and-concurrency-model]].
- [ ] Write tests: `activate` fires exactly once on `DRIVER_OK` and a `0` write
      resets (no KVM); `register_ioevent` and `register_irqfd` succeed against a
      real `VmFd` (KVM-gated, skips cleanly without `/dev/kvm`).

#### Success Criteria

- A `DRIVER_OK` transition activates the backend once and a reset tears it down,
  proven without KVM.
- The ioeventfd and irqfd bind to a real `VmFd` under the KVM-gated test.

### Phase 4: Guest discovery and slot allocation

Generate the discovery cmdline token and define the fixed device-slot table,
confirmed against the guest memory map.

#### Tasks

- [ ] Add a fixed device-slot array (base `0xd0000000`, `0x1000` stride, GSI from
      5 per slot — pending the memory-map confirmation in Open Questions); a
      device claims the next free slot at construction.
- [ ] Generate the `virtio_mmio.device=<size>@<addr>:<irq>` token per configured
      device and append it to the kernel cmdline in `vmm::Vmm::new`, preserving an
      explicit user `--cmdline` (append, never override) before
      `boot::write_cmdline`.
- [ ] Guard the memory map: assert or reject a `--mem` whose RAM top would reach
      the virtio window base (or the TSS at `0xFFFB_D000`) so guest RAM cannot
      collide with the MMIO window.
- [ ] In `vmm::Vmm::new`, after the IRQ-chip/PIT/memory steps, construct each
      transport, register its eventfds, insert it into the `MmioBus`, and subscribe
      it to the event loop; do none of this when no device is configured.
- [ ] Write tests: the token renders exactly (`0x1000@0xd0000000:5` shape); the
      cmdline appends rather than replaces; the RAM-versus-window guard rejects an
      over-large `--mem`.

#### Success Criteria

- The generated token matches the format Linux `virtio_mmio.c` parses.
- With no device configured, `Vmm::new` produces a byte-for-byte unchanged boot
  path; with one configured, the slot/GSI/token triple agree.

### Phase 5: Null test device and end-to-end wiring

Add a trivial device to drive the whole substrate and prove the guest discovers
it.

#### Tasks

- [ ] Add `src/virtio/test_device.rs`: a minimal loopback/echo `VirtioDevice`
      with one queue and empty config space that, on `process_queue`, copies each
      readable buffer into the chain's writable buffer, `add_used`s it, and signals
      the `Interrupt` (final shape pending Open Questions).
- [ ] Wire an opt-in path (a hidden flag or test-only constructor) to instantiate
      the test device so the transport runs end to end without a real backend.
- [ ] Add a virtqueue round-trip test that notifies the queue and asserts the used
      ring and the loopback buffer contents.
- [ ] Add a KVM-gated integration test (extend `tests/boot_e2e.rs`) asserting the
      guest probes the virtio-mmio device (its banner appears in kernel output)
      without crashing.
- [ ] Confirm `cargo clippy`, `cargo fmt --check`, and the full suite pass; the
      KVM-gated and e2e tests skip cleanly without `/dev/kvm`.

#### Success Criteria

- A guest QueueNotify wakes `process_queue`, the loopback completes, and the
  injected interrupt reaches the guest.
- The guest kernel logs the probed device under the KVM-gated e2e test; the build
  stays green without KVM.

## Open Questions

### 1. Confirm the MMIO window base, stride, and GSI against the memory map

- **a** (recommended) — Keep base `0xd0000000`, `0x1000` stride, and GSIs from 5
  (design Q1=a, Firecracker-aligned), and add an explicit guard so guest RAM can
  never reach the window: reject or clamp a `--mem` whose RAM top exceeds the
  window base. Today `memory.rs` builds one contiguous region from `0` with no
  hole and nothing bounds `--mem`, so a `--mem` above roughly 3.25 GiB would
  overrun `0xd0000000` (and eventually the TSS near 4 GiB). GSI 5 clears the
  legacy ISA lines (PIT 0, COM1 4) and sits within the IOAPIC's 24 inputs.
- **b** — Add a real MMIO hole in the RAM region (split the memory slot around the
  window) so large `--mem` relocates RAM above the window, as production VMMs do.
- **other** — *(write-in)*

**Decision:** a — keep base `0xd0000000`, `0x1000` stride, and GSIs from 5, and add a guard that rejects or clamps a `--mem` whose RAM top would reach the window.

### 2. Whether to depend on the unpublished virtio-device crate or define the trait in-crate

- **a** (recommended) — Define `VirtioDevice` and `Interrupt` in-crate (mirroring
  the rust-vmm trait's shape) and depend only on `virtio-queue` 0.17.0 and
  `virtio-bindings` 0.2.7. The singular `virtio-device` crate is not on crates.io
  (only `virtio-devices` plural at 0.1.0 and `dbs-virtio-devices` exist), and the
  trait is this substrate's one product, so owning it fits the crate's
  minimal-dependency, add-it-when-code-uses-it stance.
- **b** — Pull `virtio-device` as a pinned git dependency from `rust-vmm/vm-virtio`
  to reuse its status/feature helpers verbatim, accepting a git dependency.
- **other** — *(write-in)*

**Decision:** a — define `VirtioDevice` and `Interrupt` in-crate and depend only on `virtio-queue` 0.17.0 and `virtio-bindings` 0.2.7; the singular `virtio-device` crate is unpublished.

### 3. How the vCPU-thread MMIO dispatch coordinates with the coarse device mutex

- **a** (recommended) — Config-plane register access on the vCPU thread takes the
  device `Mutex` briefly (per MMIO exit) and the I/O thread takes it for
  `process_queue`; `InterruptStatus` stays an `AtomicU32` so the guest ISR read
  and the completion set never contend on the lock. Data-plane rate never touches
  the mutex except inside `process_queue`, so contention is low (design Q3=a).
- **b** — Use `try_lock` on the vCPU side and defer/retry to guarantee the vCPU
  thread never blocks on the I/O thread, at the cost of more complex register
  handling.
- **other** — *(write-in)*

**Decision:** a — config-plane register access on the vCPU thread takes the device `Mutex` briefly per MMIO exit, the I/O thread takes it for `process_queue`, and `InterruptStatus` stays an `AtomicU32`.

### 4. Whether guest memory moves from GuestMemoryMmap to GuestMemoryAtomic

- **a** (recommended) — Share guest memory with the backend as
  `GuestMemoryAtomic<GuestMemoryMmap>` (the type `virtio-queue` and the design
  expect), changing `memory::build`/`Vmm` to hold the atomic wrapper and cloning a
  handle to the I/O thread. Confirm this composes with the existing single-region
  build and does not disturb `kernel::load`/`boot`, which take `&GuestMemoryMmap`.
- **b** — Keep `GuestMemoryMmap` and share it behind a separate `Arc`, converting
  only where `virtio-queue` demands the atomic wrapper, to minimize churn in
  `memory.rs`.
- **other** — *(write-in)*

**Decision:** a — share guest memory with the backend as `GuestMemoryAtomic<GuestMemoryMmap>`, confirming it composes with the single-region build and leaves `kernel::load`/`boot` (which take `&GuestMemoryMmap`) undisturbed.

### 5. What the minimal test device should be

- **a** (recommended) — A loopback/echo device with a single queue and empty
  config space that copies readable buffers into writable buffers on
  `process_queue`. It exercises the full data plane (avail ring, chain walk, used
  ring, interrupt) with no host resource and no device-class semantics.
- **b** — A minimal virtio-blk-shaped stub (one queue, a fixed-size in-memory
  backing, a capacity config field) so the test doubles as an early block smoke
  test ahead of [[0004-block-storage-via-virtio-blk]].
- **other** — *(write-in)*

**Decision:** a — a loopback/echo device with a single queue and empty config space that copies readable buffers into writable buffers on `process_queue`.

## File Changes

| File | Action | Description |
|------|--------|-------------|
| `crates/naos-linux/src/mmio.rs` | Create | `MmioBus`: `[base, len)` to device routing; `read`/`write` with offset translation and unmapped no-op. |
| `crates/naos-linux/src/virtio/mod.rs` | Create | virtio module root; re-exports the transport, trait, and interrupt. |
| `crates/naos-linux/src/virtio/transport.rs` | Create | Modern virtio-mmio register file: identity, windowed features, queue config, status handshake, config-space delegation. |
| `crates/naos-linux/src/virtio/device.rs` | Create | `VirtioDevice` trait and `Interrupt` handle (irqfd `EventFd` plus `Arc<AtomicU32>` `InterruptStatus`). |
| `crates/naos-linux/src/virtio/queue.rs` | Create | Glue over `virtio-queue`: descriptor-chain walk honoring `NEXT`/`WRITE`, `add_used`. |
| `crates/naos-linux/src/virtio/test_device.rs` | Create | Null/loopback `VirtioDevice` to drive the transport end to end. |
| `crates/naos-linux/src/vcpu.rs` | Modify | Add `MmioRead`/`MmioWrite` arms routing to `MmioBus`; thread the bus handle in; retarget the unhandled-exit test. |
| `crates/naos-linux/src/vmm.rs` | Modify | Construct transports, register ioeventfd/irqfd, insert into the bus, subscribe to the event loop, append the discovery token. |
| `crates/naos-linux/src/memory.rs` | Modify | Hand out `GuestMemoryAtomic<GuestMemoryMmap>` for cross-thread sharing; guard `--mem` against the MMIO window (pending Open Questions). |
| `crates/naos-linux/src/main.rs` | Modify | Declare the `mmio` and `virtio` modules. |
| `crates/naos-linux/tests/boot_e2e.rs` | Modify | Add a KVM-gated assertion that the guest probes the virtio-mmio device. |
| `crates/naos-linux/Cargo.toml` | Modify | Add `virtio-queue` 0.17.0 and `virtio-bindings` 0.2.7 with rationale comments. |

## Testing Plan

- [ ] `MmioBus` routing: adjacent-range dispatch, unmapped-address no-op, edge
      offset translation (no KVM).
- [ ] Transport registers: MagicValue/Version/DeviceID constants, full 64-bit
      feature windowing, legal-only `Status` sequence, `activate` once on
      `DRIVER_OK`, reset on `0` (no KVM).
- [ ] Virtqueue plumbing: hand-built descriptor chains honor `NEXT`/`WRITE`,
      correct `(head, len)` in the used ring, malformed chains rejected (no KVM).
- [ ] Cmdline token render and append-not-override; RAM-versus-window guard
      rejects an over-large `--mem` (no KVM).
- [ ] KVM-gated: `register_ioevent` and `register_irqfd` succeed against a real
      `VmFd`; skips cleanly without `/dev/kvm`.
- [ ] Virtqueue round-trip through the loopback device: notify, then assert used
      ring plus buffer contents (no KVM for the ring logic).
- [ ] KVM-gated e2e (`tests/boot_e2e.rs`): the guest kernel probes the device in
      its boot log without crashing; skips without `/dev/kvm` or a test kernel.

## References

- [[0003-virtio-mmio-device-model]] — source design (Detailed Design, Testing
  Strategy, decided Open Questions Q1–Q4)
- [[0001-event-loop-and-concurrency-model]] — hard dependency: event-loop thread,
  ioeventfd, irqfd
- [[0004-block-storage-via-virtio-blk]], [[0005-guest-networking-and-ssh]] —
  downstream consumers of the `VirtioDevice` trait
- ADRs: [[0004-virtio-over-mmio-device-transport]] (transport choice),
  [[0003-event-driven-epoll-concurrency-model]] (concurrency model),
  [[0002-microvm-first-incremental-milestone-ladder]] (milestone ladder),
  [[0005-root-filesystem-initramfs-then-virtio-blk]]
- Code: `crates/naos-linux/src/vcpu.rs` (exit dispatch), `vmm.rs` (init order,
  IRQ chip plus PIT), `memory.rs` (guest memory), `boot.rs` (cmdline, e820,
  identity map), `main.rs` (CLI), `tests/boot_e2e.rs`
- Crates: `virtio-queue` 0.17.0 (`QueueT`/`QueueOwnedT`, `DescriptorChain`),
  `virtio-bindings` 0.2.7 (constants), `vm-memory` 0.17.1
  (`GuestMemoryAtomic`), `kvm-ioctls` 0.25 (`register_ioevent`/`register_irqfd`)
- Virtio 1.2 spec: §2.1 (device status), §2.2 (feature bits), §2.6/§2.7 (split
  virtqueues), §4.2.2 (virtio-mmio register layout)
