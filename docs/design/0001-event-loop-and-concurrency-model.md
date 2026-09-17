---
id: DESIGN-0001
title: "Event loop and concurrency model"
status: Draft
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# DESIGN 0001: Event loop and concurrency model

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
  - [1. Threading model](#1-threading-model)
  - [2. The event loop and the subscriber interface](#2-the-event-loop-and-the-subscriber-interface)
  - [3. Interrupt injection: irqfd](#3-interrupt-injection-irqfd)
  - [4. Guest notification: ioeventfd](#4-guest-notification-ioeventfd)
  - [5. Shutdown and coordination protocol](#5-shutdown-and-coordination-protocol)
  - [6. Error and panic handling](#6-error-and-panic-handling)
  - [7. Shared state model](#7-shared-state-model)
- [API / Interface Changes](#api--interface-changes)
- [Data Model](#data-model)
- [Testing Strategy](#testing-strategy)
- [Migration / Rollout Plan](#migration--rollout-plan)
- [Open Questions](#open-questions)
  - [1. Event loop: event-manager or hand-rolled](#1-event-loop-event-manager-or-hand-rolled)
  - [2. vCPU wakeup mechanism](#2-vcpu-wakeup-mechanism)
  - [3. Loop thread topology](#3-loop-thread-topology)
  - [4. Shared-device locking granularity](#4-shared-device-locking-granularity)
  - [5. Fatal-error and panic teardown policy](#5-fatal-error-and-panic-teardown-policy)
  - [6. Multi-vCPU forward-compatibility](#6-multi-vcpu-forward-compatibility)
- [References](#references)
<!--toc:end-->

## Overview

This design replaces naos-linux's single blocking `vcpu.run()` loop with an
**event-driven concurrency model**: the vCPU runs on its own thread, and host-side
I/O readiness is serviced by an epoll event loop. It is the substrate every later
milestone stands on — serial input, virtio-blk, and virtio-net are all "a file
descriptor plus a callback" hung off this loop. It is deliberately **device-agnostic**:
it defines the threading model, the shutdown protocol, and the generic interrupt
(irqfd) and notification (ioeventfd) primitives, and nothing device-specific.

Because everything depends on it, this is the milestone where robustness is dialed
in *first*. Getting the shutdown and wakeup protocol right here is the difference
between a VMM that is sturdy and pleasant to use and one that hangs, leaks the
terminal, or wedges on the first corner case. The interactive serial console
([[0002-interactive-serial-console]]) is the first consumer and doubles as this
design's functional acceptance test.

## Goals and Non-Goals

### Goals

- Move the vCPU to a dedicated thread (still blocking in `KVM_RUN`) and service
  host I/O on an **epoll event loop** on a second thread, per
  [[0003-event-driven-epoll-concurrency-model]].
- Define a **shutdown and coordination protocol** that terminates *both* threads
  cleanly no matter which side initiates (guest halt/reset, host error, operator
  request), and always restores host resources (the terminal, in later designs).
- Provide two generic KVM fast-path primitives every device will use:
  **irqfd** (inject a guest interrupt from an `EventFd` with no userspace
  round-trip) and **ioeventfd** (turn a guest MMIO/PIO write into an `EventFd`
  signal instead of a vmexit).
- Define the minimal **subscriber interface** a device registers with the loop
  (an fd + a readiness callback), so adding a device never touches the loop core.
- Establish the **shared-state model** — `Arc` guest memory, `Arc<Mutex<_>>`
  devices — and the ownership/lifetime rules that keep KVM's fd drop-order
  invariants intact across threads.
- Propagate a fatal error on either thread into a clean whole-VM teardown and a
  non-zero exit.

### Non-Goals

- **Any concrete device.** The UART stays output-only in this milestone; serial
  input is [[0002-interactive-serial-console]]. No virtio
  ([[0003-virtio-mmio-device-model]] onward).
- **Multiple vCPUs.** One vCPU thread. The loop must not *preclude* N vCPU threads
  later (see Open Questions), but SMP is not built here.
- **An async runtime (tokio).** Mismatched to `KVM_RUN`'s blocking ioctl and the
  explicit control the VMM needs.
- **Serial input, initramfs, CLI changes.** Those arrive with the first consumer.
  This milestone is behavior-preserving: the VM still boots to the init panic and
  exits 0 (see Migration).

## Background

Today `vmm::Vmm::run` calls `vcpu::run`, a single thread that loops on
`VcpuFd::run` (blocking `KVM_RUN`) and dispatches `VcpuExit::IoOut`/`IoIn`, `Hlt`,
`Shutdown`, and recognized reset-port writes. There is exactly one I/O source (the
UART, output-only), so a single blocking loop suffices and nothing ever needs to
run *while the vCPU is blocked inside `KVM_RUN`*.

That breaks the moment a second I/O source exists. Host stdin becomes readable
asynchronously; a virtio backend must service a queue when the guest notifies it;
a device must inject an interrupt when its backend completes. None of these can be
observed from inside a blocked `KVM_RUN`. `WALK-linux.md` §13 names this the pivot:
*"Once you have an event loop, every future device becomes cheap."* The cost is
real concurrency — two threads over shared guest memory — which is why the
correctness protocol, not the epoll plumbing, is the substance of this document.

## Detailed Design

### 1. Threading model

Three threads: a supervisor (the main thread), a dedicated event-loop thread, and
a vCPU thread (topology per Open Questions §3).

```text
 main thread (supervisor)
   Vmm::new()  (KVM/VM/mem/vCPU setup)
   build event loop + exit_evt; register irqfd/ioeventfd for devices
   spawn vCPU thread ─────────┐        spawn event-loop thread ─────────┐
   join both; teardown guards │                                         │
   return exit status         ▼                                         ▼
                        vCPU thread                             event-loop thread
                        loop KVM_RUN:                           EventManager::run:
                          PIO/MMIO  → dispatch (lock device)      subscriber fd → callback
                          Hlt/reset → break                       exit_evt      → break
                          other     → fatal; break
                          on break: signal exit_evt              on break: signal exit_evt
```

- **Main thread (supervisor).** Owns `Vmm` (`Kvm`, `VmFd`, the `Arc` guest
  memory), spawns the vCPU and event-loop threads, then blocks joining both. It
  does nothing else for a single VM today, but it is where the process lifecycle
  and a future per-VM control socket ([[0007-management-and-control-api]]) live —
  kept separate from the VM's I/O loop, the way Firecracker keeps its API thread
  separate from its VMM thread. Multiple VMs on a host come from running one such
  process per VM (Firecracker's model, and what per-VM jailing needs), not from
  this thread hosting several — so multi-VM is a control-plane concern above the
  process, not a change to this substrate.
- **vCPU thread.** Owns the `VcpuFd`, blocks in `KVM_RUN`, and handles synchronous
  exits (device port/MMIO access, halt, reset). The only thread that touches the
  `VcpuFd`.
- **Event-loop thread.** Owns the epoll fd (the `event-manager` `EventManager`,
  Open Questions §1) and dispatches subscriber callbacks on readiness.
- **Ownership and lifetime.** KVM requires `VmFd` to outlive `VcpuFd`, and guest
  memory to outlive `VmFd`. `Vmm` stays on the supervisor thread; the `VcpuFd` is
  *moved* into the vCPU thread. The supervisor **joins both threads before** `Vmm`
  drops, so the KVM fd drop-order invariant holds across the thread boundaries.

### 2. The event loop and the subscriber interface

The loop is intentionally tiny: it owns an epoll fd, a table of registered
`(RawFd → Subscriber)`, and it blocks in `epoll_wait`, calling the matching
subscriber on readiness. A device is registered by handing the loop an fd and a
callback; the loop knows nothing about what the device does.

```text
trait Subscriber {
    // Called when one of the subscriber's registered fds is ready.
    fn process(&mut self, events: Events, ops: &mut LoopOps);
}
```

Whether we adopt rust-vmm's `event-manager` (which already provides this
`Subscriber`/`EventManager` shape, epoll edge cases handled) or a thin hand-rolled
wrapper over `vmm-sys-util`'s epoll helpers is the central decision of this design
(Open Questions §1). Either way the loop core stays under \~100 lines and every
device plugs in the same way.

### 3. Interrupt injection: irqfd

Devices must raise guest interrupts from the event-loop thread without a vmexit.
KVM's **irqfd** binds an `EventFd` to a GSI on the in-kernel IRQ chip: writing the
eventfd injects the interrupt entirely in the kernel.

- `VmFd::register_irqfd(&event_fd, gsi)` wires an `EventFd` to a GSI. The in-kernel
  IRQ chip (already created in `Vmm::new` — this is *why* the MVP created it
  "before anything raises interrupts") routes it to the vCPU.
- Any device holds the write end; pulsing it (e.g. vm-superio's `Trigger`, a
  virtio backend on completion) injects the IRQ with zero userspace round-trip.
- The event loop never touches interrupt delivery directly — it just runs the
  backend code that pulses the eventfd. This keeps the loop device-agnostic.

### 4. Guest notification: ioeventfd

The mirror of irqfd for the guest→host direction. KVM's **ioeventfd** binds an
`EventFd` to a guest MMIO/PIO address (optionally a datamatch): a guest write to
that address signals the eventfd *instead of* producing a vmexit.

- `VmFd::register_ioevent(&event_fd, &io_addr, datamatch)` registers it.
- The device's backend registers that eventfd as a loop subscriber; the guest's
  notification wakes the backend directly. This is what makes a virtio QueueNotify
  cheap ([[0003-virtio-mmio-device-model]]), but it is defined here as a generic
  primitive so the transport design just *uses* it.
- Not every access can be an ioeventfd (config reads still take a vmexit on the
  vCPU thread); ioeventfd is the fast-path for high-frequency notify writes only.

### 5. Shutdown and coordination protocol

This is the correctness core. Shutdown must (a) terminate both threads, (b) fire
from either side, (c) be idempotent, and (d) always run resource-restoration
guards. A shared `exit_evt: EventFd` (registered as a loop subscriber) plus an
`AtomicBool` stop flag carry it.

- **Guest-initiated (the normal path).** The vCPU thread breaks its loop on `Hlt`,
  `Shutdown`, or a recognized reset-port write (the existing `is_reset_request`
  path — a guest `poweroff` lands here), then writes `exit_evt`. The event-loop
  thread wakes on `exit_evt` and returns from `epoll_wait`; the supervisor joins
  both threads and returns exit 0.
- **Host-initiated (error, or operator request in a later design).** The loop must
  stop a vCPU that may be blocked inside `KVM_RUN`. It sets the stop flag, calls
  `VcpuFd::set_kvm_immediate_exit(1)`, and sends the vCPU thread a signal with a
  no-op handler (`SIGUSR1`); the in-flight `KVM_RUN` returns promptly, the vCPU
  thread observes the stop flag and breaks. (Which wakeup mechanism is robust on
  our kernel is Open Questions §2.)
- **Idempotency.** `exit_evt` is an eventfd counter and the stop flag is atomic;
  both sides may signal, and a second signal is harmless. Join happens exactly
  once on the main thread.
- **Teardown guards.** Host state that must be restored (the terminal in
  [[0002-interactive-serial-console]], tap devices later) is held in RAII guards
  owned by the main thread, so restoration runs on the normal return, on an error
  return, and on a panic unwind.

### 6. Error and panic handling

- A fatal error on the vCPU thread (an unexpected exit, a failed ioctl) breaks the
  loop, records the error, and signals `exit_evt`; `Vmm::run` returns it so `main`
  prints the chain and exits non-zero — the MVP's error contract, preserved.
- A fatal error on a subscriber signals `exit_evt` and stops the vCPU (as above).
- **Thread panic.** If the vCPU thread panics, the join observes it; the main
  thread converts it into an error return so the guards still run (no naked
  `unwrap` of the `JoinHandle`). A poisoned `Mutex` on a shared device is treated
  as fatal — the VM is no longer trustworthy — not silently recovered.

### 7. Shared state model

- **Guest memory** becomes `Arc<GuestMemoryMmap>`: KVM holds the mapping
  independently via `set_user_memory_region`, but both threads and every future
  device backend need a handle. `Arc` is the idiomatic rust-vmm share.
- **Devices** shared between the vCPU thread (servicing synchronous port/MMIO
  exits) and the loop thread (servicing backends) are `Arc<Mutex<_>>`. Lock hold
  time is a register access or a short buffer push; contention is negligible at
  the frequencies this milestone cares about (Open Questions §4).
- **Coordination:** `exit_evt: EventFd`, an `AtomicBool` stop flag, and the
  subscriber registry. GSI and ioeventfd registrations are recorded so later
  devices allocate without collisions.

## API / Interface Changes

- **No CLI or output change.** Same flags, same boot-to-panic behavior, same exit
  codes. This milestone is invisible to a user running `just run`.
- **`vmm::Vmm::run`** stops calling `vcpu::run` directly: it spawns the vCPU
  thread (moving in the `VcpuFd`) and the event-loop thread, then joins both and
  returns the run result.
- **New internal module** (`event_loop` / `io`): the loop, the `Subscriber` trait,
  and helpers to register irqfd/ioeventfd. `vcpu::run` keeps its exit-dispatch
  semantics but gains the stop-flag check and the `exit_evt` signal on break.
- **New dependency:** `event-manager` (Open Questions §1, decided); `vmm-sys-util`
  (already present) supplies `EventFd` and epoll.

## Data Model

- **Shared runtime state:** `Arc<GuestMemoryMmap>`; `exit_evt: EventFd`; an
  `Arc<AtomicBool>` stop flag; the subscriber registry `RawFd → Box<dyn Subscriber>`.
- **KVM registrations:** irqfd bindings (`EventFd` ↔ GSI) and ioeventfd bindings
  (`EventFd` ↔ guest address), each recorded so devices allocate GSIs/addresses
  without collision.
- **No guest-physical-memory-map change.** irqfd/ioeventfd are host-side KVM
  objects; the guest memory map is untouched until a device claims an MMIO window.

## Testing Strategy

Robustness is the point of this milestone, so the tests target the protocol, not
just the happy path. The project's conventions carry over: co-located
`#[cfg(test)]` unit tests, KVM-gated tests that skip cleanly without `/dev/kvm`,
and a gated end-to-end check.

- **Unit — event loop core.** A subscriber registered on an `EventFd` has its
  callback invoked when the fd is signalled; `exit_evt` breaks the loop;
  registering/removing subscribers behaves. No KVM needed (pure eventfd/epoll).
- **Unit — shutdown protocol (KVM-gated).** With a vCPU whose guest immediately
  `hlt`s: the guest-initiated path signals `exit_evt`, the loop returns, and
  `run` yields `Ok`. With a guest that spins: the host-initiated path
  (`set_kvm_immediate_exit` + signal) breaks the in-flight `KVM_RUN` and the vCPU
  thread stops. Double-signalling `exit_evt` is idempotent.
- **Unit — irqfd/ioeventfd registration (KVM-gated).** `register_irqfd` on a GSI
  and `register_ioevent` on an address succeed against a real `VmFd`.
- **Corner cases (KVM-gated).** vCPU thread panic → `run` returns an error and the
  join does not deadlock; an unexpected vCPU exit tears down the loop; a fatal
  subscriber error stops the vCPU.
- **Behavior-preserving integration.** The existing boot-to-panic e2e (kernel, no
  device changes, serial still output-only) runs under the new threaded loop and
  still exits 0 — proof the concurrency change is invisible.
- **Functional acceptance.** The real end-to-end exercise of input + interrupt +
  shutdown through the loop is [[0002-interactive-serial-console]]; this design is
  considered validated when that milestone's acceptance test passes on top of it.

**Coverage target** matches the crate: ~full coverage of the pure loop logic;
KVM-bound registration and the run loop covered by the gated + e2e tests.

## Migration / Rollout Plan

One behavior-preserving step, landable on its own with the tree green:

1. **Threaded loop, no new device.** Introduce the vCPU thread, the event loop,
   `exit_evt`, and the shutdown protocol with the UART still output-only and no
   subscribers except `exit_evt`. The VM boots to the init panic and exits 0 —
   *no user-visible change*, but the threading, shutdown, and teardown are now
   proven in isolation, before any device depends on them. The MVP e2e is the
   regression gate.

Only after this is solid do consumers land: serial input + interrupt
([[0002-interactive-serial-console]]), then the virtio substrate
([[0003-virtio-mmio-device-model]]). Each new device is an additive subscriber
registration; none reopen the loop core.

## Open Questions

Each item is a decision to settle before this design moves from Draft to
Approved. Option **a** is the recommendation; **b** onward are alternatives;
**other** is a write-in. Record the choice on the **Decision** line.

### 1. Event loop: event-manager or hand-rolled

The load-bearing decision — this substrate carries every future device.

- **a (recommended).** Adopt rust-vmm's `event-manager`. It is proven in
  production VMMs (Cloud Hypervisor), already provides the `Subscriber`/`EventManager`
  shape, and handles the epoll edge cases we would otherwise re-discover as bugs.
  For the lynchpin, robustness outweighs minimalism, and this is the same
  "clearly worse to write ourselves" call that justified `kvm-ioctls`.
- **b.** A thin hand-rolled epoll wrapper over `vmm-sys-util`. Fewer dependencies
  and fully owned, truer to "minimum viable everything" — but we own every corner
  case, on the one component that must not be fragile.
- **other.** *(write-in)*

**Decision:** a — event-manager.

### 2. vCPU wakeup mechanism

How the host side breaks an in-flight `KVM_RUN`.

- **a (recommended).** `set_kvm_immediate_exit(1)` plus a no-op `SIGUSR1` to the
  vCPU thread; confirm it reliably interrupts `KVM_RUN` on our kernel and settle
  where the handler is installed.
- **b.** No forced wakeup — rely only on the guest's own `Hlt`/reset to end the
  loop; simplest, but cannot stop a wedged or compute-bound guest.
- **other.** *(write-in)*

**Decision:** a — immediate-exit + no-op SIGUSR1.

### 3. Loop thread topology

- **a.** vCPU on a spawned thread; the main thread *becomes* the event loop (two
  contexts, no third thread). Simplest — the Firecracker / rust-vmm-reference
  shape — but it couples the process's main thread to a single VM's runtime.
- **b (recommended).** vCPU thread + a dedicated event-loop thread, with the main
  thread supervising. The Cloud Hypervisor shape and a natural fit for
  `event-manager` (§1); "run one VM" is already "one VM's runtime under a trivial
  supervisor," so a control plane / multi-VM
  ([[0007-management-and-control-api]]) is additive, not a substrate rewrite.
  Costs one mostly-idle thread and a three-way shutdown now.
- **other.** *(write-in)*

**Decision:** b — dedicated event-loop thread; the main thread supervises.

### 4. Shared-device locking granularity

- **a (recommended).** `Arc<Mutex<Device>>` per shared device; contention is
  negligible at this milestone's frequencies. Revisit only under measured
  contention.
- **b.** Finer-grained splits (config vs data path, lock-free rings) up front.
- **other.** *(write-in)*

**Decision:** a — coarse `Arc<Mutex>` per device (config + queue), with an atomic interrupt status. Firecracker's model, verified against source (see [[0003-virtio-mmio-device-model]]).

### 5. Fatal-error and panic teardown policy

- **a (recommended).** Any thread's fatal error signals `exit_evt`; the main
  thread joins, runs teardown guards, and returns non-zero. A vCPU-thread panic is
  converted to an error return (guards still run); a poisoned device mutex is
  fatal, not recovered.
- **b.** Let a panic abort the process (simpler, but skips terminal/resource
  restoration — unacceptable once the terminal is in raw mode).
- **other.** *(write-in)*

**Decision:** a — signal, join, run guards, non-zero exit; panic to error.

### 6. Multi-vCPU forward-compatibility

- **a (recommended).** Implement a single vCPU thread now, but keep the loop and
  shutdown protocol agnostic to vCPU count (a set of vCPU threads sharing one I/O
  loop and one `exit_evt`) so SMP is additive later.
- **b.** Bake in single-vCPU assumptions and refactor when SMP is actually built.
- **other.** *(write-in)*

**Decision:** a — stay vCPU-count-agnostic; implement one.

## References

- ADRs: [[0003-event-driven-epoll-concurrency-model]] (the decision this design
  implements), [[0002-microvm-first-incremental-milestone-ladder]] (milestone
  context).
- Consumers / dependents: [[0002-interactive-serial-console]] (first consumer +
  functional test), [[0003-virtio-mmio-device-model]] (uses irqfd/ioeventfd),
  [[0004-block-storage-via-virtio-blk]], [[0005-guest-networking-and-ssh]].
- `DESIGN-naos-linux.md`, `WALK-linux.md` (§8 vCPU loop, §13 "What's next").
- KVM API — `Documentation/virt/kvm/api.html`: `KVM_IRQFD`, `KVM_IOEVENTFD`,
  `KVM_SET_KVM_IMMEDIATE_EXIT`, in-kernel IRQ chip.
- rust-vmm crates: `event-manager` (`Subscriber`, `EventManager`), `vmm-sys-util`
  (`EventFd`, epoll), `kvm-ioctls` (`VmFd::register_irqfd`, `register_ioevent`,
  `VcpuFd::set_kvm_immediate_exit`).
