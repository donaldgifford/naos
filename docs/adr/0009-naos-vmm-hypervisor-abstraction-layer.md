---
id: ADR-0009
title: "naos-vmm hypervisor abstraction layer"
status: Proposed
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# 0009. naos-vmm hypervisor abstraction layer

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

Proposed

> **Tracked, not yet accepted.** This is `ARCHITECTURE.md`'s stage 3, recorded
> here as a decision-in-waiting. It is intentionally not accepted until both
> concrete backends exist.

## Context

A core naos goal is a layer that sits atop Linux/KVM and macOS/Hypervisor.
framework so backend code is shared rather than duplicated. `ARCHITECTURE.md`
defines this as `naos-vmm`: a hypervisor abstraction **extracted from**
`naos-linux` and `naos-macos` once both exist and work — not designed up front.
Premature abstraction is the single failure mode the project most explicitly
rejects.

## Decision

Record the intent to create **`naos-vmm`**, the hypervisor abstraction trait,
extracted from the two concrete backends. Do not accept yet.

Accept once stage 2 (`naos-macos`) exists and works, so the trait can be
extracted from what the two backends *actually* share. Both backends are then
refactored to implement it, and future backends fit behind the same interface.

In the meantime, keep the Linux backend's module boundaries clean (a device
notion, an MMIO/PIO bus, the event loop) because those are the seams the trait
will later follow — but do **not** create the trait or code to an imagined
interface.

## Consequences

### Positive

- The eventual abstraction is grounded in reality, not speculation.
- The Linux backend keeps evolving concretely and fast.

### Negative

- Some Linux-side structure may need reshaping when the trait lands — expected
  and acceptable.

### Neutral

- Until then, there is deliberate duplication-in-waiting (only one backend
  exists, so there is nothing to share yet).

## Alternatives Considered

- **Design the trait now.** Rejected — premature abstraction.
- **Contribute a Hypervisor.framework backend to Cloud Hypervisor** instead of
  building our own abstraction. A legitimate alternative noted in
  `ARCHITECTURE.md`; not taken, for ownership and learning reasons.

**Blocking dependency:** stage 2, `naos-macos`.

## References

- `ARCHITECTURE.md` — "Project ladder" (stage 3), "Relationship to other VMM
  projects" (Cloud Hypervisor, libkrun)
- [[0001-naos-is-one-platform-learning-and-homelab]]
