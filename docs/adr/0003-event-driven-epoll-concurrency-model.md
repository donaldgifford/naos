---
id: ADR-0003
title: "Event-driven (epoll) concurrency model"
status: Accepted
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# 0003. Event-driven (epoll) concurrency model

<!--toc:start-->
- [Status](#status)
- [Context](#context)
- [Decision](#decision)
- [Consequences](#consequences)
  - [Positive](#positive)
  - [Negative](#negative)
  - [Neutral](#neutral)
- [Alternatives Considered](#alternatives-considered)
- [References](#references)
<!--toc:end-->

## Status

Accepted

## Context

The MVP runs a single blocking `vcpu.run()` loop on one thread, and the UART is
output-only. The moment we want to *type* into the guest (serial RX), or run any
virtio device (which must service virtqueues and inject interrupts when a
host-side backend becomes ready), or handle any second I/O source, we have to
multiplex vCPU execution with host-side file-descriptor readiness. The current
single-blocking-loop model cannot express that.

`WALK-linux.md` flagged this explicitly: *"Once you have an event loop, every
future device becomes cheap."* This is the pivotal architectural change that
unblocks every milestone after M1.

## Decision

Adopt an **epoll-based event loop** as the VMM's core concurrency model:

- The vCPU runs on its **own thread**, blocking in `KVM_RUN` as today.
- Host-side I/O sources — serial stdin, virtio device backends, eventfds — are
  registered with an **epoll loop** on a separate thread and serviced on
  readiness.
- Device interrupts are delivered to the guest via **KVM irqfd** where possible,
  so the kernel signals the guest without a userspace round-trip.
- Use rust-vmm's `event-manager` (or a thin epoll wrapper). This dependency now
  earns its place under the "no dependencies we wouldn't write ourselves" bar,
  because a second I/O source genuinely forces it.

## Consequences

### Positive

- Unlocks serial input and every virtio device; new devices become incremental.
- Matches the architecture of Firecracker and Cloud Hypervisor.

### Negative

- Introduces multithreading and its correctness burden: shared guest memory,
  synchronization, and a defined shutdown/coordination protocol between the
  vCPU thread and the I/O thread.
- The simplicity of the single blocking loop is gone.

### Neutral

- `event-manager` (or an epoll wrapper) becomes a core dependency, and a
  threading model is now baked into the VMM.

## Alternatives Considered

- **Keep the single blocking loop and poll stdin between vCPU exits.** Rejected:
  racy, and it cannot service virtqueues asynchronously.
- **Full async runtime (tokio).** Rejected: heavyweight and mismatched to
  `KVM_RUN`'s blocking ioctl and the tight, explicit control the VMM needs.
- **Hand-rolled epoll instead of `event-manager`.** Viable; revisit only if
  `event-manager` proves a poor fit.

## References

- [[0002-microvm-first-incremental-milestone-ladder]]
- [[0001-event-loop-and-concurrency-model]]
- `WALK-linux.md` §13 "What's next"
