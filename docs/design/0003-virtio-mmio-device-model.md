---
id: DESIGN-0003
title: "virtio-mmio device model"
status: Draft
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# DESIGN 0003: virtio-mmio device model

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
  - [2. The virtio-mmio transport register model](#2-the-virtio-mmio-transport-register-model)
  - [3. Split virtqueues and the data plane](#3-split-virtqueues-and-the-data-plane)
  - [4. The device interface](#4-the-device-interface)
  - [5. Config plane and data plane on the event loop](#5-config-plane-and-data-plane-on-the-event-loop)
  - [6. Guest discovery, memory map, and IRQ routing](#6-guest-discovery-memory-map-and-irq-routing)
- [API / Interface Changes](#api--interface-changes)
- [Data Model](#data-model)
- [Testing Strategy](#testing-strategy)
- [Migration / Rollout Plan](#migration--rollout-plan)
- [Open Questions](#open-questions)
  - [1. MMIO base, per-device stride, and GSI](#1-mmio-base-per-device-stride-and-gsi)
  - [2. virtio-device and virtio-queue API surface](#2-virtio-device-and-virtio-queue-api-surface)
  - [3. Transport locking granularity](#3-transport-locking-granularity)
  - [4. Device-slot table shape](#4-device-slot-table-shape)
- [References](#references)
<!--toc:end-->

## Overview

This design specifies the *device-agnostic* virtio substrate: the MMIO bus that
routes guest device accesses, the modern virtio-mmio transport register model, a
split-virtqueue data plane, and the fixed MMIO/GSI layout the guest discovers
from the kernel command line. It is a foundation, not a device — it carries no
block or net logic. Its one product is a `VirtioDevice` trait that
[[0004-block-storage-via-virtio-blk]] and [[0005-guest-networking-and-ssh]]
implement to get a working device without touching the transport, the bus, or
the eventfds. This is the sibling of the event loop
([[0001-event-loop-and-concurrency-model]]): the shared machinery whose
correctness must be dialed in *before* any consumer depends on it.

## Goals and Non-Goals

### Goals

- Handle `KVM_EXIT_MMIO` in `vcpu::run` and route guest MMIO reads/writes to the
  device registered for the faulting range, via a small `MmioBus` — replacing the
  current defensive `bail!` arm.
- Implement the modern (virtio 1.x) **virtio-mmio transport** per virtio 1.2
  §4.2.2: register interface, 64-bit feature negotiation, queue configuration, the
  device-status handshake.
- Drive **split virtqueues** (virtio 1.2 §2.6/§2.7): descriptor table plus
  available and used rings in guest RAM, walked by a generic device backend.
- Make the data plane cheap: wake a backend on a guest QueueNotify through an
  **ioeventfd** and signal completion through an **irqfd** on a per-device GSI, so
  the fast path never takes a userspace MMIO round-trip. The threading and eventfd
  machinery lives in [[0001-event-loop-and-concurrency-model]].
- Define the **`VirtioDevice` trait** — identity, feature bits, config space,
  queue handling — that every device implements and the transport consumes.
- Establish guest discovery (`virtio_mmio.device=<size>@<addr>:<irq>`), a fixed
  MMIO window above the 1 GiB boot identity map, and per-device GSI allocation.

### Non-Goals

- **Any concrete device.** virtio-blk is [[0004-block-storage-via-virtio-blk]];
  virtio-net is [[0005-guest-networking-and-ssh]]. This doc stops at the trait.
- **virtio-pci or legacy virtio.** virtio-mmio, modern only, per
  [[0004-virtio-over-mmio-device-transport]]. No PCI config space, no BAR
  allocation, no bus enumeration.
- **Multiple queues per device, indirect descriptors, or packed virtqueues.**
  The substrate is built for one split virtqueue per device; extra feature bits
  stay negotiated off until a device needs them.
- **Device hotplug or a dynamic allocator.** The MMIO window is a fixed table of
  device slots; `vm-allocator` is deferred.
- **MMIO window relocation.** A hardcoded base/stride/GSI layout, per the ADR's
  "opinions, not options."

## Background

The MVP (`DESIGN-naos-linux.md`) boots a kernel to a panic with one PIO device
(the 16550 UART) and a single blocking vCPU loop. The event-loop design
([[0001-event-loop-and-concurrency-model]]) then splits the vCPU onto its own
thread, services host-side I/O and eventfds on readiness, and delivers device
interrupts through KVM irqfd. This substrate assumes that machinery and builds
the shared device foundation on top of it.

Today `vcpu.rs` handles only `IoOut`/`IoIn` (PIO), `Hlt`, `Shutdown`, and a reset
request; any MMIO exit falls through to the defensive `bail!` arm (the
`run_errors_on_an_unhandled_exit` test relies on exactly that). `vmm.rs` already
creates an in-kernel IRQ chip and PIT before vCPU creation, so GSI routing through
the IOAPIC is available for free — only the `register_irqfd` binding is new. Guest
RAM (`memory.rs`) is one contiguous region from physical 0 with no MMIO hole; this
substrate adds the first MMIO devices, sited above guest RAM so no hole is needed.
The transport is settled by [[0004-virtio-over-mmio-device-transport]]:
virtio-mmio, modern, no PCI, each device a fixed MMIO region plus a dedicated IRQ
line, discovered from the kernel command line — Firecracker's microVM model.
`WALK-linux.md` §13 flags the MMIO bus and IRQ routing as "the single biggest
unlock" that every future device depends on.

## Detailed Design

The through-line is a clean split between a low-frequency **config plane**
(transport registers, serviced on the vCPU thread) and a high-frequency **data
plane** (queue notifications and completions, serviced on the I/O thread with no
userspace MMIO exit on the fast path). A device backend sees neither the registers
nor the eventfds — only the trait in part 6.

### 1. The MMIO bus and vCPU dispatch

KVM reports a guest access to an unbacked physical address as
`VcpuExit::MmioRead(addr, data)` or `VcpuExit::MmioWrite(addr, data)`. Two new
arms are added to the `match` in `vcpu::run`:

```text
MmioRead(addr, data)  => bus.read(addr, data),   // fill `data` from device
MmioWrite(addr, data) => bus.write(addr, data),  // apply `data` to device
```

A small `MmioBus` owns an ordered map of `[base, base+len)` ranges to device
handles. `read`/`write` find the range containing `addr`, translate to a register
offset (`addr - base`), and dispatch to that device's transport. An access that
matches no range returns zeroes / is ignored, mirroring the existing "return 0xFF
for unknown PIO ports" convention — the guest probes addresses we don't back and
we must not crash. The bus is deliberately tiny; it is address routing and
nothing else. It replaces the current unconditional `bail!` for MMIO exits, so
that defensive test is updated to point at a still-unmapped address.

Note the boot-time identity page tables in `boot.rs` cover only the first 1 GiB,
and the virtio-mmio window lives above that (see part 5). This is fine: those
tables exist only to get the guest kernel to `startup_64`, after which the kernel
installs its own page tables and `ioremap`s device regions before it ever touches
them.

### 2. The virtio-mmio transport register model

The transport is the register file the guest's `drivers/virtio/virtio_mmio.c`
programs to bring a device up. It implements the modern MMIO register layout
(virtio 1.2 §4.2.2). One transport instance wraps one `VirtioDevice`; the
load-bearing registers:

| Offset | Name | Access | Purpose |
| ------ | ---- | ------ | ------- |
| 0x000 | MagicValue | R | `0x74726976` ("virt", little-endian) |
| 0x004 | Version | R | `2` — modern, non-legacy |
| 0x008 | DeviceID | R | device type from the backend (2 = block, 1 = net, …) |
| 0x00c | VendorID | R | naos vendor id (informational) |
| 0x010 / 0x014 | DeviceFeatures / Sel | R / W | 64-bit device feature bits, windowed |
| 0x020 / 0x024 | DriverFeatures / Sel | W / W | bits the driver accepts, windowed |
| 0x030 | QueueSel | W | select the queue the queue regs address |
| 0x034 | QueueNumMax | R | max descriptors we support for the selected queue |
| 0x038 | QueueNum | W | size the driver chose |
| 0x044 | QueueReady | RW | 1 = queue live |
| 0x050 | QueueNotify | W | driver kicks the device (data-plane doorbell) |
| 0x060 / 0x064 | InterruptStatus / ACK | R / W | pending-interrupt bits / ack |
| 0x070 | Status | RW | device-status handshake byte |
| 0x080–0x0a4 | Queue{Desc,Driver,Device}{Low,High} | W | guest addresses of the three rings |
| 0x0fc | ConfigGeneration | R | config-space consistency counter |
| 0x100+ | Config | R(W) | device-specific config, delegated to the backend |

**Feature negotiation** (virtio 1.2 §2.2) is windowed through the `*Sel`
registers because the feature space is 64 bits wide and each data register is 32
bits. The transport always advertises `VIRTIO_F_VERSION_1` (bit 32) ORed with the
device's `device_features()`. The driver reads `DeviceFeatures` in both windows,
writes the intersection it accepts to `DriverFeatures`, then sets `FEATURES_OK`;
the transport hands the accepted set to the backend via `ack_features`.

**Status handshake** (virtio 1.2 §2.1). The driver walks the `Status` byte
through `ACKNOWLEDGE (1)` → `DRIVER (2)` → `FEATURES_OK (8)` → `DRIVER_OK (4)`. We
validate the transitions, clear `FEATURES_OK` on an unacceptable feature set, and
set `DEVICE_NEEDS_RESET (64)` on a protocol error. `DRIVER_OK` is the trigger to
*activate* the backend (part 6). A write of `0` resets the device and tears the
queues down.

Config-plane register reads/writes are infrequent (probe, feature negotiation,
reset) and run synchronously on the vCPU thread through the `MmioBus`. The state
they touch is shared with the I/O thread (part 5) behind a lock.

### 3. Split virtqueues and the data plane

A split virtqueue (virtio 1.2 §2.6/§2.7) is three structures the driver allocates
in guest RAM, whose guest-physical addresses it hands us via the `Queue*`
registers:

- **Descriptor table** (§2.7.5): an array of `(addr, len, flags, next)`
  descriptors. Each describes one guest buffer; `VIRTQ_DESC_F_NEXT` chains them
  and `VIRTQ_DESC_F_WRITE` marks a device-writable buffer.
- **Available ring** (§2.7.6): driver → device. The driver publishes the head
  descriptor index of each new request here and bumps `avail.idx`.
- **Used ring** (§2.7.8): device → driver. The device publishes completed head
  indices plus bytes-written here and bumps `used.idx`.

We do not hand-roll ring parsing. The rust-vmm `virtio-queue` crate provides
`Queue` and `DescriptorChain` iteration over any `vm-memory` `GuestMemory`; it
handles index wrapping, bounds-checks buffers against guest RAM, and publishes to
the used ring. Guest memory is shared with the I/O thread as a
`GuestMemoryAtomic<GuestMemoryMmap>` so a backend can read/write guest buffers
safely while the vCPU thread runs.

The data-plane loop, end to end, is device-agnostic — the transport walks the
queue and the backend interprets each chain:

```text
guest driver: publish head idx to avail ring, bump avail.idx,
              write queue index to QueueNotify (0x050)
        │
        ▼  KVM matches an ioeventfd on QueueNotify (addr + datamatch on queue idx):
           no userspace MMIO exit — the eventfd fires and the guest keeps running
        │
        ▼  I/O thread (event loop) wakes on that eventfd → device.process_queue(idx)
backend:  Queue::iter(mem) → for each DescriptorChain:
            interpret the request, fill device-writable buffers,
            Queue::add_used(head, bytes_written)
        │
        ▼  set InterruptStatus bit 0 (used-buffer notification), then
           write 1 to this device's irqfd EventFd
        │
        ▼  KVM injects the IRQ via the in-kernel IOAPIC; guest ISR reads
           InterruptStatus (MMIO exit → transport), drains the used ring,
           writes InterruptACK to clear the bit
```

The two eventfds are the crux of the ADR's "inject through irqfd" and the reason
the data plane is cheap; both are created and wired on the event loop
([[0001-event-loop-and-concurrency-model]]):

- **ioeventfd** on QueueNotify. `kvm-ioctls` `VmFd::register_ioevent` binds an
  `EventFd` to MMIO address `0x050` within the device window with a datamatch on
  the queue index. A guest write there fires the eventfd inside the kernel and
  the guest keeps running — no exit to `vcpu::run`, no `MmioBus` dispatch.
- **irqfd** on the device GSI. `kvm-ioctls` `VmFd::register_irqfd` binds an
  `EventFd` to the device's GSI. The backend injects an interrupt by writing to
  it; KVM and the already-created in-kernel IRQ chip do the rest — no
  `KVM_IRQ_LINE` ioctl per completion.

### 4. The device interface

The whole point of the substrate is that a device implements one trait and gets a
working virtio-mmio device for free. The transport owns the register file, the
status handshake, feature windowing, and the `Queue` machinery; the backend
supplies only its identity, feature set, config space, and per-queue handling.
The shape mirrors rust-vmm's `virtio-device` `VirtioDevice` trait:

```rust
pub trait VirtioDevice: Send {
    /// DeviceID register (virtio 1.2 §5): 2 = block, 1 = net, ...
    fn device_type(&self) -> u32;

    /// Feature bits offered; the transport ORs in VIRTIO_F_VERSION_1.
    fn device_features(&self) -> u64;
    /// The subset the driver accepted, recorded after FEATURES_OK.
    fn ack_features(&mut self, acked: u64);

    /// Per-queue max sizes (QueueNumMax); its len is the queue count.
    fn queue_max_sizes(&self) -> &[u16];

    /// Device-specific config space at MMIO offset 0x100+ (blk capacity, net MAC).
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, offset: u64, data: &[u8]);

    /// Called once on DRIVER_OK: the device receives its live queues, guest
    /// memory, and an Interrupt over the irqfd, and owns its data plane after.
    fn activate(
        &mut self,
        mem: GuestMemoryAtomic<GuestMemoryMmap>,
        queues: Vec<Queue>,
        irq: Interrupt,
    );

    /// Data-plane entry: invoked on the event loop when the QueueNotify
    /// ioeventfd for `idx` fires. Walk the queue, complete work, signal `irq`.
    fn process_queue(&mut self, idx: u16);
}
```

`Interrupt` is a thin wrapper over the device's irqfd `EventFd` plus the
`AtomicU32` `InterruptStatus`: `signal()` sets the used-buffer bit and writes the
eventfd. A device never sees an MMIO offset, a status byte, or a GSI number. That
is the contract [[0004-block-storage-via-virtio-blk]] and
[[0005-guest-networking-and-ssh]] code against.

### 5. Config plane and data plane on the event loop

The transport-plus-device is one object shared as `Arc<Mutex<…>>` between two
threads whose roles are defined in [[0001-event-loop-and-concurrency-model]]:

- the **vCPU thread**, which reaches it through the `MmioBus` for config-plane
  register reads/writes (probe, feature negotiation, `Status`, reset), and
- the **I/O thread**, which reaches it as an event-loop subscriber that runs
  `process_queue` when the QueueNotify ioeventfd fires.

`InterruptStatus` is the one field touched from both sides at data-plane rate — an
ISR-side read on the vCPU thread, a set-on-completion on the I/O thread — so it is
an `AtomicU32` rather than mutex-guarded state. The remaining register/queue state
is guarded by the device mutex. The exact locking granularity (one mutex for the
whole device versus a finer split) is in Open Questions; the substrate does not
re-implement the epoll loop or the irqfd delivery path, only subscribes to it.

### 6. Guest discovery, memory map, and IRQ routing

There is no PCI bus, so the guest cannot probe for a device; it is told where to
look on the kernel command line (virtio 1.2 §4.2.2, and Linux's
`drivers/virtio/virtio_mmio.c` module parameter). The VMM emits one token per
device:

```text
virtio_mmio.device=<size>@<addr>:<irq>
# e.g.  virtio_mmio.device=0x1000@0xd0000000:5
```

Each token maps a single virtio-mmio window and wires its interrupt. The VMM and
the cmdline must agree on the triple. The fixed layout (a hardcoded decision per
the ADR's "opinions, not options"; exact values in Open Questions):

```text
Guest physical address space (MMIO window)
 …
 <top of guest RAM>          contiguous RAM region (memory.rs), no MMIO hole
 …                           (unbacked — MMIO accesses above RAM vmexit)
 0xd000_0000  slot 0  ── 0x1000 (one page) ── GSI 5
 0xd000_1000  slot 1  ── 0x1000            ── GSI 6
 0xd000_2000  slot 2  ── 0x1000            ── GSI 7
 …            a fixed table of device slots, one page and one GSI each
```

The window sits far above guest RAM and above the 1 GiB boot identity map, so
accesses to it always trap as MMIO and never collide with a memory slot; no MMIO
hole in the RAM region is required and `memory.rs` is unchanged. Each GSI is a
real IOAPIC input on the in-kernel IRQ chip `vmm.rs` already creates, so no new
IRQ infrastructure is needed — only the `register_irqfd` binding per device.

`Vmm::new` gains, after the existing IRQ-chip/PIT/memory steps and before the run
loop: for each configured device, construct its transport, register the
QueueNotify ioeventfd and the GSI irqfd with the `VmFd`, insert the transport into
the `MmioBus`, subscribe it to the event loop, and append its
`virtio_mmio.device=…` token to the cmdline. With no device configured, none of
this happens and the prior boot path runs byte-for-byte unchanged.

## API / Interface Changes

- **`vcpu::run` signature** gains an `MmioBus` (or a shared handle to it) and two
  new match arms, `MmioRead`/`MmioWrite`. No change to the PIO, `Hlt`, `Shutdown`,
  or reset behavior.
- **New internal modules.** An `mmio` module (`MmioBus`) and a `virtio` module
  (the transport register model, the `VirtioDevice` trait, `Interrupt`, and the
  `virtio-queue` glue). Devices live in their own modules and depend on `virtio`.
- **Cmdline injection.** When a device is configured, the VMM appends its
  `virtio_mmio.device=<size>@<addr>:<irq>` token to the kernel command line. An
  explicit user `--cmdline` is preserved; virtio tokens are appended, not
  overridden. The device-specific flags that *cause* a device to be configured
  (for example a block `--drive`) are defined by the consumer docs, not here.
- **No new binary flags in this doc.** The substrate is machinery; the CLI surface
  that instantiates devices belongs to [[0004-block-storage-via-virtio-blk]] and
  [[0005-guest-networking-and-ssh]].

## Data Model

- **In guest memory:** the driver allocates the descriptor table, available ring,
  and per-request buffers (virtio 1.2 §2.7), which the backend reads; the backend
  writes the used ring and device-writable buffers. naos never allocates
  guest-side structures.
- **In VMM memory:** per device, the transport register file (feature windows,
  queue addresses/size/ready, `Status`, `AtomicU32` `InterruptStatus`), the
  `virtio-queue` `Queue` state, and the ioeventfd/irqfd `EventFd`s. The `MmioBus`
  holds the `[base, len)` → transport routing table.
- **Device-specific state** (a backing file, a tap fd, config-space contents) is
  owned by the concrete device and opaque to the transport. Nothing in the
  substrate is persisted across runs.

## Testing Strategy

Following the house pattern (`DESIGN-naos-linux.md`): unit tests where the logic
is fiddly, plus KVM-gated checks that skip cleanly without `/dev/kvm`.

- **MMIO bus routing** (no KVM): adjacent `[base, len)` ranges dispatch to the
  right handle, an unmapped address is a no-op not a panic, and offset translation
  (`addr - base`) is correct at range edges.
- **Transport registers** (no KVM): behind a mock `VirtioDevice`,
  MagicValue/Version/DeviceID read back the right constants; the feature-select
  windows expose the full 64-bit space; the `Status` handshake advances only
  through the legal `ACKNOWLEDGE→DRIVER→FEATURES_OK→DRIVER_OK` sequence,
  `activate` fires exactly once on `DRIVER_OK`, and a `0` write resets. Pure
  functions of the register file, like the GDT/e820 tests in `boot.rs`.
- **Virtqueue plumbing** (no KVM): back a `GuestMemoryMmap` with hand-built
  descriptor chains; assert the transport honors `NEXT`/`WRITE` flags, publishes
  the right `(head, len)` to the used ring, and rejects malformed chains.
- **KVM-gated** (skip cleanly without `/dev/kvm`): `register_ioevent` and
  `register_irqfd` succeed against a real `VmFd`.
- **End-to-end** is proven by the first consumer: [[0004-block-storage-via-virtio-blk]]
  booting to a serial login exercises the whole substrate. This doc ships the unit
  and KVM-gated layers; the consumer adds the e2e check.

## Migration / Rollout Plan

Incremental, each step independently observable, so a regression is bisectable to
one commit — the "each milestone de-risks the next" discipline of the ADR ladder
([[0002-microvm-first-incremental-milestone-ladder]]):

1. **MMIO bus + dispatch.** Add `MmioBus` and the `MmioRead`/`MmioWrite` arms.
   With no device registered, unmapped MMIO is a no-op; the prior boot still
   works. Update `run_errors_on_an_unhandled_exit` to target a still-unmapped
   address.
2. **Transport shell.** Register a virtio-mmio window whose DeviceID/feature/
   status registers respond behind a stub `VirtioDevice`, with no real queue. Boot
   a kernel with `CONFIG_VIRTIO_MMIO=y` and confirm the guest *probes* the device
   (visible in dmesg) without crashing.
3. **Queues + eventfds.** Bring up the `Queue`, register the ioeventfd and irqfd,
   subscribe the transport to the event loop. Verify a guest QueueNotify wakes
   `process_queue` and an injected interrupt reaches the guest ISR.
4. **Trait handoff.** Freeze the `VirtioDevice` trait and the `Interrupt` handle
   against a trivial loopback device, so [[0004-block-storage-via-virtio-blk]] and
   [[0005-guest-networking-and-ssh]] can start against a stable interface.

The substrate is additive: with no device configured the boot path is byte-for-byte
identical to before, so this milestone cannot regress the ones beneath it.

## Open Questions

Each item is a decision to settle before this design moves from Draft to Approved.
Option **a** is the recommendation; **b** onward are alternatives; **other** is a
write-in. Record the choice on the **Decision** line. Block- and net-specific
questions (drive-flag naming, in-place reboot, write durability) live in the
consumer docs.

### 1. MMIO base, per-device stride, and GSI

- **a (recommended).** Keep the proposed `0xd0000000` base, `0x1000` stride, and
  GSIs from 5, but confirm against the IOAPIC's available GSIs and the guest
  kernel's expectations before hardcoding — the "confirm the addresses" caveat the
  MVP raised for GDT/page-table placement.
- **b.** Adopt Firecracker's exact virtio-mmio layout verbatim to minimize
  novelty.
- **other.** *(write-in)*

**Decision:** *pending*

### 2. virtio-device and virtio-queue API surface

- **a (recommended).** Pin the exact traits and queue helpers against the
  published `virtio-device` / `virtio-queue` versions before coding; use what the
  crates provide and hand-write only the virtio-mmio register decode they do not.
- **b.** Hand-roll the register decode and queue iteration, using the crates only
  for the descriptor types.
- **other.** *(write-in)*

**Decision:** *pending*

### 3. Transport locking granularity

- **a (recommended).** Start coarse — one `Mutex` over the whole transport-plus-
  device, with `InterruptStatus` as an `AtomicU32`; revisit only if the vCPU
  thread measurably contends with the I/O thread.
- **b.** A finer split up front: config behind a mutex, the ring lock-free, the
  interrupt atomic.
- **other.** *(write-in)*

**Decision:** *pending*

### 4. Device-slot table shape

- **a (recommended).** A small fixed array of MMIO slots (base + stride + GSI per
  index), sized to the near-term device count; a device claims the next free slot
  at construction.
- **b.** Introduce `vm-allocator` now and allocate MMIO ranges and GSIs
  dynamically, anticipating hotplug.
- **other.** *(write-in)*

**Decision:** *pending*

## References

- ADRs: [[0002-microvm-first-incremental-milestone-ladder]],
  [[0003-event-driven-epoll-concurrency-model]],
  [[0004-virtio-over-mmio-device-transport]],
  [[0005-root-filesystem-initramfs-then-virtio-blk]]
- Sibling designs: [[0001-event-loop-and-concurrency-model]] (prerequisite event
  loop, irqfd/ioeventfd, threading), [[0004-block-storage-via-virtio-blk]] and
  [[0005-guest-networking-and-ssh]] (consumers of this trait)
- `DESIGN-naos-linux.md` (MVP scope, house style); `WALK-linux.md` §2 (memory
  map), §13 (What's next); current code in `crates/naos-linux/src/`: `vcpu.rs`
  (exit dispatch), `vmm.rs` (init order, IRQ chip + PIT), `memory.rs`, `boot.rs`
- Virtio 1.2 spec: §2.1 (device status), §2.2 (feature bits), §2.6/§2.7 (split
  virtqueues), §4.2.2 (virtio-mmio register layout)
- Guest kernel config: `CONFIG_VIRTIO`, `CONFIG_VIRTIO_MMIO` (device-specific
  symbols belong to the consumer docs)
- rust-vmm crates: `virtio-device`, `virtio-queue`, `vm-memory`
  (`GuestMemoryAtomic`), `kvm-ioctls` (`register_ioevent` / `register_irqfd`),
  `event-manager`, `vmm-sys-util` (`EventFd`)
