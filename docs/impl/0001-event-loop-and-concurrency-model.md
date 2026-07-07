---
id: IMPL-0001
title: "Event loop and concurrency model"
status: Draft
author: Donald Gifford
created: 2026-07-06
---
<!-- markdownlint-disable-file MD025 MD041 -->

# IMPL 0001: Event loop and concurrency model

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
  - [Phase 1: Behavior-preserving threading](#phase-1-behavior-preserving-threading)
    - [Tasks](#tasks)
    - [Success Criteria](#success-criteria)
  - [Phase 2: The event loop thread](#phase-2-the-event-loop-thread)
    - [Tasks](#tasks-1)
    - [Success Criteria](#success-criteria-1)
  - [Phase 3: Shutdown and wakeup protocol](#phase-3-shutdown-and-wakeup-protocol)
    - [Tasks](#tasks-2)
    - [Success Criteria](#success-criteria-2)
  - [Phase 4: Device-facing primitives](#phase-4-device-facing-primitives)
    - [Tasks](#tasks-3)
    - [Success Criteria](#success-criteria-3)
  - [Phase 5: Testing corner cases and docs](#phase-5-testing-corner-cases-and-docs)
    - [Tasks](#tasks-4)
    - [Success Criteria](#success-criteria-4)
- [Open Questions](#open-questions)
  - [1. Event manager subscriber container and version pin](#1-event-manager-subscriber-container-and-version-pin)
  - [2. SIGUSR1 handler placement and vCPU thread wakeup](#2-sigusr1-handler-placement-and-vcpu-thread-wakeup)
  - [3. Serial path placement in this milestone](#3-serial-path-placement-in-this-milestone)
  - [4. Surfacing vCPU thread errors to the supervisor](#4-surfacing-vcpu-thread-errors-to-the-supervisor)
- [File Changes](#file-changes)
- [Testing Plan](#testing-plan)
- [References](#references)
<!--toc:end-->

## Objective

Replace naos-linux's single blocking `KVM_RUN` loop with the threaded
concurrency substrate every later milestone stands on: a supervisor `main`
thread, a dedicated `event-manager` epoll loop on its own thread, and a vCPU
thread that still blocks in `KVM_RUN`. This milestone lands the shutdown and
wakeup protocol (`exit_evt` + `set_kvm_immediate_exit` + a `SIGUSR1` wakeup),
the clean three-way join and teardown ordering, and the device-facing irqfd /
ioeventfd primitives plus the coarse `Arc<Mutex<dyn VirtioDevice>>` shared-state
model — all with no new device and no user-visible change: the VM still boots to
the init panic and exits 0.

**Implements:** [[0001-event-loop-and-concurrency-model]]

## Scope

### In Scope

- Move the vCPU loop (`vcpu::run`) onto a dedicated thread; `main` becomes the
  supervisor that spawns, joins, and owns process lifecycle.
- Stand up a dedicated event-loop thread on rust-vmm's `event-manager`
  (`EventManager`), with the shutdown `exit_evt` as its only subscriber this
  milestone.
- The shutdown and coordination protocol: `exit_evt: EventFd`, an
  `Arc<AtomicBool>` stop flag, `VcpuFd::set_kvm_immediate_exit` plus a no-op
  `SIGUSR1` to break an in-flight `KVM_RUN`, idempotent signalling, and a clean
  join of both threads before `Vmm` drops.
- The shared-state model: `Arc<GuestMemoryMmap>` guest memory, the coarse
  `Arc<Mutex<dyn VirtioDevice>>` device model with `InterruptStatus` as an
  `Arc<AtomicU32>` (Firecracker's model), and RAII teardown guards owned by the
  supervisor.
- Device-facing KVM primitives with no consumer yet: `register_irqfd` and
  `register_ioevent` helpers on the `VmFd`, plus the subscriber-registration
  entry point a future device uses.
- Error and panic propagation from the vCPU thread into a non-zero supervisor
  exit, with teardown guards still run.

### Out of Scope

- Any concrete device. The UART stays output-only; serial input is
  [[0002-interactive-serial-console]]. No virtio device implementation
  ([[0003-virtio-mmio-device-model]] onward) — only the trait shape and the
  eventfd primitives it will use.
- MMIO-bus dispatch and the virtio-mmio transport register model
  ([[0003-virtio-mmio-device-model]]); the vCPU loop keeps its defensive `bail!`
  arm for unhandled exits.
- Multiple vCPUs (SMP). One vCPU thread; the protocol stays vCPU-count-agnostic
  so SMP is additive later, but it is not built here.
- Any CLI, cmdline, initramfs, or output change. This milestone is
  behavior-preserving.

## Current State

Today `vmm::Vmm::run` calls `vcpu::run(&mut self.vcpu, &mut self.serial)` — a
single synchronous thread that loops on `VcpuFd::run` (blocking `KVM_RUN`) and
dispatches `VcpuExit::IoOut` / `IoIn` (serial via `serial::handle_write` /
`serial::handle_read`, reset via `is_reset_request`), `Hlt` / `Shutdown`, and
`bail!`s on anything else. `serial.rs` is the vm-superio 16550 UART wired to
`Stdout` (output-only; `EventFdTrigger` exists but is never pulsed). `Vmm` owns
`_kvm`, `_vm`, `_guest_mem: GuestMemoryMmap`, `vcpu: VcpuFd`, and `serial`, and
relies on struct field drop order to satisfy KVM's `VmFd`-outlives-`VcpuFd` and
memory-outlives-`VmFd` invariants. There is exactly one I/O source and nothing
needs to run while the vCPU is blocked inside `KVM_RUN`, so a single blocking
loop suffices — which is exactly what breaks the moment a second I/O source
(serial input, a virtio backend) exists.

## Dependencies

- **New crate:** `event-manager` (rust-vmm), the epoll event loop and its
  `MutEventSubscriber` / `EventManager` shapes (Q1=a, decided). Latest is the
  `~0.4` line; exact pin and subscriber container type are an Open Question.
- **Already present:** `vmm-sys-util` supplies `EventFd` and epoll; `kvm-ioctls`
  supplies `VmFd::register_irqfd`, `VmFd::register_ioevent`, and
  `VcpuFd::set_kvm_immediate_exit`; `libc` supplies `sigaction` / `pthread_kill`
  for the wakeup signal.
- **Nothing blocks this** — it is the substrate. IMPL-0002
  ([[0002-interactive-serial-console]]), IMPL-0003
  ([[0003-virtio-mmio-device-model]]), IMPL-0004
  ([[0004-block-storage-via-virtio-blk]]), and IMPL-0005
  ([[0005-guest-networking-and-ssh]]) all depend on **this** milestone: the
  threading model, the shutdown protocol, and the irqfd / ioeventfd primitives.

## Implementation Phases

Each phase is a shippable increment that keeps the build green. The arc goes
from behavior-preserving threading, through the event loop and the shutdown
protocol, to the device-facing primitives, then hardening and tests.

```text
 main thread (supervisor)
   Vmm::new()  (KVM/VM/mem/vCPU setup, install SIGUSR1 handler)
   build event loop + exit_evt; register irqfd/ioeventfd for future devices
   spawn vCPU thread ─────────┐        spawn event-loop thread ─────────┐
   join both; run teardown    │                                         │
   return exit status         ▼                                         ▼
                        vCPU thread                             event-loop thread
                        loop KVM_RUN:                           EventManager::run:
                          PIO/MMIO  -> dispatch (lock device)     subscriber fd -> callback
                          Hlt/reset -> break                      exit_evt      -> stop
                          other     -> fatal; break
                          on break: signal exit_evt              on stop: signal vCPU
```

### Phase 1: Behavior-preserving threading

Move the blocking loop off the main thread with no other change: the vCPU runs
on a spawned thread, `main` supervises, and shared state moves behind `Arc`. The
VM still boots to the init panic and exits 0.

#### Tasks

- [ ] Change `Vmm::_guest_mem` to `Arc<GuestMemoryMmap>`; clone the `Arc` into
  the vCPU thread while `memory::register`, `kernel::load`, and `boot::*` keep
  taking `&GuestMemoryMmap` via `Arc` deref.
- [ ] Refactor `Vmm::run` to spawn a `std::thread` that owns the moved-in
  `VcpuFd` (and, for now, the `serial` device) and calls the `vcpu::run`-shaped
  loop, returning its `Result` through the `JoinHandle`.
- [ ] Guarantee the supervisor joins the vCPU thread before `Vmm`'s `VmFd` and
  `Arc<GuestMemoryMmap>` drop, preserving KVM's fd drop-order invariant across
  the thread boundary; document the ordering.
- [ ] Convert a vCPU-thread panic into an `anyhow` error return (match on
  `JoinHandle::join`'s `Err`), never a naked `unwrap` of the handle.
- [ ] Keep `main` unchanged in behavior: `Vmm::new` then `Vmm::run`, same exit
  codes.
- [ ] Write tests: a KVM-gated test that a spawned vCPU thread whose guest
  immediately `hlt`s joins with `Ok`; a non-KVM test that a panicking vCPU
  closure surfaces as an error from the supervisor rather than aborting.

#### Success Criteria

- `cargo build -p naos-linux` and `cargo clippy` pass.
- `boot_e2e.rs` still reaches the init panic and exits 0 under the threaded loop.
- The panic-to-error test passes without deadlocking the join.

### Phase 2: The event loop thread

Stand up the `event-manager` epoll loop on its own thread and introduce the
shutdown `exit_evt`. This milestone registers only `exit_evt` as a subscriber;
serial stays inline on the vCPU thread (output-only).

#### Tasks

- [ ] Add `event-manager` to `crates/naos-linux/Cargo.toml`.
- [ ] Create a new module (`event_loop`) holding the `EventManager` setup and the
  `MutEventSubscriber` wrapper the loop dispatches.
- [ ] Define an exit subscriber implementing `MutEventSubscriber` whose `init`
  registers `exit_evt` with `EventOps` and whose `process` flips the loop's stop
  flag so the event-loop thread returns from its `EventManager::run` drive loop.
- [ ] Spawn the event-loop thread from the supervisor; add the exit subscriber
  via `EventManager::add_subscriber`.
- [ ] Wire the vCPU thread's loop break to write `exit_evt` so the event-loop
  thread wakes and exits; have the supervisor join both threads.
- [ ] Write non-KVM unit tests for the loop core: a subscriber registered on an
  `EventFd` has its `process` invoked when the fd is signalled, and signalling
  `exit_evt` makes the event-loop thread return.

#### Success Criteria

- The event-loop thread starts, blocks in `epoll_wait`, and returns when
  `exit_evt` is signalled.
- `boot_e2e.rs` still exits 0 with both threads joined.
- The loop-core unit tests pass without `/dev/kvm`.

### Phase 3: Shutdown and wakeup protocol

Make shutdown fire cleanly from either side. The host side breaks an in-flight
`KVM_RUN` with `set_kvm_immediate_exit` plus a no-op `SIGUSR1`; guest halt,
shutdown, and reset all land on a clean exit 0.

#### Tasks

- [ ] Add an `Arc<AtomicBool>` stop flag shared by the vCPU thread and the event
  loop; make `exit_evt` writes and the flag idempotent.
- [ ] Install a no-op `SIGUSR1` handler once at startup (via `libc::sigaction`),
  and publish the vCPU thread's `pthread_t` so the host side can `pthread_kill`
  it.
- [ ] Implement host-initiated stop: set the stop flag, call
  `VcpuFd::set_kvm_immediate_exit(1)`, send `SIGUSR1`; the vCPU loop observes the
  interrupted `KVM_RUN` (EINTR), checks the flag, and breaks.
- [ ] On any vCPU break (Hlt, Shutdown, `is_reset_request`, or a fatal exit)
  write `exit_evt`; on the event-loop side, drive the same host-initiated stop so
  both threads terminate regardless of which side initiates.
- [ ] Preserve behavior: Hlt / Shutdown / reset yield exit 0; an unexpected exit
  or failed ioctl yields a non-zero exit with the `anyhow` chain intact.
- [ ] Ensure the join happens exactly once on the supervisor and teardown guards
  run on normal, error, and panic-unwind paths.
- [ ] Write KVM-gated tests: a guest that `hlt`s takes the guest-initiated path
  and `run` yields `Ok`; a guest that spins is stopped by the host-initiated path
  (`set_kvm_immediate_exit` + `SIGUSR1`); double-signalling `exit_evt` is
  harmless.

#### Success Criteria

- A spinning guest is stopped promptly by the host-initiated path in the gated
  test (no hang, no timeout).
- Guest-initiated shutdown still exits 0; unexpected exits still exit non-zero.
- `boot_e2e.rs` remains the passing regression gate.

### Phase 4: Device-facing primitives

Land the KVM fast-path primitives and the shared-state interfaces every device
will use, with no real device behind them yet.

#### Tasks

- [ ] Add helpers wrapping `VmFd::register_irqfd(&EventFd, gsi)` and
  `VmFd::register_ioevent(&EventFd, &IoEventAddress, NoDatamatch)`.
- [ ] Record GSI and ioeventfd registrations in the `Vmm` so later devices
  allocate GSIs and addresses without collision.
- [ ] Define the coarse shared-device model: a placeholder `VirtioDevice` trait
  held as `Arc<Mutex<dyn VirtioDevice>>` (config plus queue together) with
  `InterruptStatus` as a separate `Arc<AtomicU32>`, matching Firecracker's model
  (Q4=a).
- [ ] Provide the subscriber-registration entry point a future device uses to
  hang an fd plus a readiness callback off the loop without touching the loop
  core.
- [ ] Write KVM-gated smoke tests: `register_irqfd` on a GSI and
  `register_ioevent` on an address succeed against a real `VmFd`; a dummy
  `Arc<Mutex<dyn VirtioDevice>>` locks and its `Arc<AtomicU32>` status updates.

#### Success Criteria

- The registration helpers compile and their gated smoke tests pass against a
  real `VmFd`.
- The device traits and shared-state types are in place with no device, and the
  loop core is untouched by them.

### Phase 5: Testing corner cases and docs

Harden the protocol, cover the failure paths, and lock in the behavior-preserving
regression.

#### Tasks

- [ ] Corner-case tests (KVM-gated where needed): vCPU-thread panic makes `run`
  return an error and the join does not deadlock; an unexpected vCPU exit tears
  down the loop; a fatal subscriber error stops the vCPU.
- [ ] Error-propagation test: a failed vCPU-thread ioctl surfaces as the
  supervisor's non-zero return with the `anyhow` chain preserved.
- [ ] Migrate the existing `vcpu.rs` tests (`run_returns_when_the_guest_halts`,
  `run_errors_on_an_unhandled_exit`) to the threaded shape.
- [ ] Run `boot_e2e.rs` unchanged as the regression gate; confirm exit 0 and the
  `Linux version` / `No working init found` assertions still hold.
- [ ] Add module and doc comments covering the threading model, the shutdown and
  wakeup protocol, and the drop-order invariants.
- [ ] Confirm `cargo test`, `cargo clippy`, and the crate's coverage target all
  pass.

#### Success Criteria

- All unit, gated, and e2e tests pass; the pure loop logic is near-fully covered.
- No markdownlint or clippy regressions; doc comments explain the concurrency
  model.

## Open Questions

Implementation-level uncertainties not settled by the design decisions, the code,
or the crate docs. The design's Open Questions (Q1–Q6) are already decided and
are honored above; these are narrower.

### 1. Event manager subscriber container and version pin

- **a** (recommended) — Parameterize `EventManager` over
  `Arc<Mutex<dyn MutEventSubscriber + Send>>` and pin `event-manager` to the
  `~0.4` line; this matches the Firecracker / Cloud Hypervisor shape and lets the
  supervisor share a subscriber's lock with a device backend. Confirm whether the
  `remote_endpoint` feature is needed (see Q2) before enabling it.
- **b** — Use `Box<dyn MutEventSubscriber>` when a subscriber need not be shared
  outside the loop thread; simpler, but forces a container change the moment a
  device is shared with the vCPU thread.
- **other** — *(write-in)*

**Decision:** *pending*

### 2. SIGUSR1 handler placement and vCPU thread wakeup

- **a** (recommended) — Install the no-op `SIGUSR1` handler process-wide with
  `libc::sigaction` in `main` before spawning any thread, capture the vCPU
  thread's `pthread_t` (via `pthread_self` inside the thread, published back
  through a `channel`), and have the host side `pthread_kill(tid, SIGUSR1)` after
  `set_kvm_immediate_exit`. Directly targets the blocking thread and matches
  rust-vmm practice.
- **b** — Skip the signal and rely on `event-manager`'s `remote_endpoint` to
  request the stop, pairing `set_kvm_immediate_exit` with a spin/flag check;
  avoids signal plumbing but leaves a window where an already-entered `KVM_RUN`
  is not interrupted until the next natural exit.
- **other** — *(write-in)*

**Decision:** *pending*

### 3. Serial path placement in this milestone

- **a** (recommended) — Keep serial PIO handling inline on the vCPU thread
  (output-only, no subscriber) for this milestone; the host stdin subscriber and
  the shared `Arc<Mutex<Serial>>` arrive with IMPL-0002
  ([[0002-interactive-serial-console]]). Behavior-preserving and the smallest
  diff.
- **b** — Move stdout flushing into an `event-manager` subscriber now to exercise
  the subscriber path end-to-end before a real device exists; more coverage, but
  adds a subscriber the design's migration says this milestone should not need.
- **other** — *(write-in)*

**Decision:** *pending*

### 4. Surfacing vCPU thread errors to the supervisor

- **a** (recommended) — Carry the run outcome as the vCPU thread's
  `JoinHandle<Result<()>>` payload, using `exit_evt` only as the cross-thread
  wakeup; this decouples "why we stopped" from "wake the other thread" and keeps
  the error's `anyhow` chain intact through the join.
- **b** — Store the result in a shared `Arc<Mutex<Option<Result<()>>>>` (or a
  `channel`) that the supervisor reads after joining; more flexible for multiple
  vCPUs later, but adds a lock and a second place the outcome can live.
- **other** — *(write-in)*

**Decision:** *pending*

## File Changes

| File | Action | Description |
|------|--------|-------------|
| crates/naos-linux/Cargo.toml | Modify | Add the `event-manager` dependency (pin per Open Question 1). |
| crates/naos-linux/src/event_loop.rs | Create | `EventManager` wiring, the `MutEventSubscriber` wrapper and exit subscriber, `register_irqfd` / `register_ioevent` helpers, the `VirtioDevice` trait plus `Arc<Mutex<_>>` / `Arc<AtomicU32>` `InterruptStatus` shared-state types, and the subscriber-registration entry point. |
| crates/naos-linux/src/vmm.rs | Modify | `_guest_mem` becomes `Arc<GuestMemoryMmap>`; `run` spawns and joins the vCPU and event-loop threads, owns `exit_evt`, the `Arc<AtomicBool>` stop flag, and RAII teardown guards, and records irqfd / ioeventfd registrations. |
| crates/naos-linux/src/vcpu.rs | Modify | The loop runs on the vCPU thread, gains the stop-flag check after `KVM_RUN` and the `exit_evt` signal on break; signature takes the shared handles; keeps the reset / Hlt / Shutdown semantics and the defensive `bail!`. |
| crates/naos-linux/src/main.rs | Modify | Install the no-op `SIGUSR1` handler before spawning; add `mod event_loop`; keep the `Vmm::new` then `Vmm::run` supervisor flow. |
| crates/naos-linux/src/serial.rs | Modify | Keep the UART output-only; make it shareable (`Arc<Mutex<Serial>>`) only if Open Question 3 resolves to a subscriber this milestone. |
| crates/naos-linux/tests/boot_e2e.rs | Modify | Unchanged in intent; runs as the behavior-preserving regression gate for the threaded loop. |

## Testing Plan

- [ ] Unit — loop core (no KVM): a subscriber on an `EventFd` has `process`
  invoked on readiness; `exit_evt` returns the event-loop thread; add/remove of
  subscribers behaves.
- [ ] Unit — supervisor (no KVM): a panicking vCPU closure surfaces as an error
  from the join, guards still run, no deadlock.
- [ ] Unit — shutdown protocol (KVM-gated): guest `hlt` takes the
  guest-initiated path and `run` is `Ok`; a spinning guest is stopped by
  `set_kvm_immediate_exit` + `SIGUSR1`; double-signalling `exit_evt` is
  idempotent.
- [ ] Unit — device primitives (KVM-gated): `register_irqfd` on a GSI and
  `register_ioevent` on an address succeed; a dummy `Arc<Mutex<dyn VirtioDevice>>`
  locks and its `Arc<AtomicU32>` status updates.
- [ ] Corner cases (KVM-gated): unexpected vCPU exit tears down the loop; a fatal
  subscriber error stops the vCPU; a failed ioctl surfaces non-zero with the
  `anyhow` chain.
- [ ] Integration / e2e (KVM-gated): `boot_e2e.rs` boots to the init panic and
  exits 0 under the threaded loop — the behavior-preserving regression gate.

## References

- [[0001-event-loop-and-concurrency-model]] — source design (Detailed Design,
  Testing Strategy, decided Open Questions Q1–Q6).
- [[0003-event-driven-epoll-concurrency-model]] — the ADR this design implements.
- [[0002-microvm-first-incremental-milestone-ladder]] — milestone context.
- [[0002-interactive-serial-console]] — first consumer and functional acceptance
  test.
- [[0003-virtio-mmio-device-model]] — uses the irqfd / ioeventfd primitives and
  the coarse `Arc<Mutex<dyn VirtioDevice>>` shared-state model landed here.
- Crate docs: `event-manager` (`EventManager`, `MutEventSubscriber`, `EventOps`,
  `Events`, `EventSet`), `vmm-sys-util` (`EventFd`, epoll), `kvm-ioctls`
  (`VmFd::register_irqfd`, `VmFd::register_ioevent`,
  `VcpuFd::set_kvm_immediate_exit`).
- Code: `crates/naos-linux/src/{main,vmm,vcpu,serial,memory}.rs`,
  `crates/naos-linux/tests/boot_e2e.rs`, `crates/naos-linux/Cargo.toml`.
