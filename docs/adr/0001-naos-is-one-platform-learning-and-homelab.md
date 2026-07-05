---
id: ADR-0001
title: "naos is one platform: learning project and homelab VMM"
status: Accepted
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# 0001. naos is one platform: learning project and homelab VMM

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

The stage-1 MVP (`naos-linux`) boots a kernel to dmesg and panics on missing
init. Because it is heavily commented and deliberately minimal, it reads like a
first-principles teaching example, and that raised a real question: is naos a
throwaway learning artifact that we would later fork into a separate
"production" VMM, or is it the actual foundation we build the real system on?

`ARCHITECTURE.md` already frames naos as a platform grown in a learning-first
style, but its project ladder stops at "extract the abstraction" (stage 3) and
never reaches "runs a workload." Read literally, the roadmap never leaves
example territory, which is what makes the codebase *feel* like a demo. We need
to settle the positioning before committing to the next stages of work.

## Decision

naos is **one platform**, not an example-versus-production fork. It is
simultaneously (a) a first-principles learning project and (b) the homelab
microVM platform the author will actually run. The same codebase serves both
and hardens in place, stage by stage.

- The heavily-commented, minimum-viable-everything style is **permanent**, not
  a phase to be "cleaned up for production." Comprehensibility and
  production-readiness are not opposites.
- **"Production for naos" means homelab-grade**: single-operator,
  comprehensible, correct, and good enough to run the author's own workloads.
  It explicitly does *not* mean hyperscale, multi-tenant, or serverless
  performance parity — those remain platform non-goals.
- We will **not** maintain a separate "production naos." There is one naos.

## Consequences

### Positive

- No throwaway code; every stage builds on the last, so learning value and the
  running system are the same artifact.
- Avoids the rewrite / premature-abstraction trap of designing a "real" system
  from imagination before the pain that motivates it has been felt.

### Negative

- Requires ongoing discipline to resist "let's start clean for production," and
  to keep the code both comprehensible *and* increasingly capable.
- Because performance and scale are deprioritized, naos will not be the right
  tool for hyperscale/multi-tenant use — by design.

### Neutral

- Firecracker, Cloud Hypervisor, and libkrun remain the right tools for use
  cases outside naos's homelab-grade bar. That is fine and expected.

## Alternatives Considered

- **Fork: keep `naos-linux` as a reference example and design a separate
  production VMM.** Rejected: throws away working, tested, understood code and
  designs "production" up front — exactly the premature-abstraction failure
  mode `ARCHITECTURE.md` is built to avoid.
- **Pure learning project, stop at stage 3.** Rejected: the author intends to
  run real workloads (homelab microVMs and general-purpose VMs), so naos must
  become a system, not end at an abstraction exercise.

## References

- `ARCHITECTURE.md` — "What naos is", "First-principles philosophy", "Project
  ladder", platform non-goals.
- [[0002-microvm-first-incremental-milestone-ladder]]
