---
id: ADR-0008
title: "Observability and metrics via rondo"
status: Proposed
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# 0008. Observability and metrics via rondo

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

> **Tracked, not yet accepted.** Recorded so the intent is not lost; blocked on
> the dependencies below.

## Context

Operating microVMs at scale needs observability: boot timings, vCPU/exit
statistics, device throughput, resource usage, and health. The intended path is
to onboard **rondo** as naos's metrics/observability system. This is not yet
load-bearing — there is no running fleet, and not yet even a persistent VM — so
it is tracked rather than built.

## Decision

Record the intent to integrate metrics and observability via **rondo**. Do not
accept yet.

Accept once (a) the management/control plane exists
([[0007-management-and-control-api]]) to host and expose metrics, and (b) there
is something worth measuring (a networked VM, and eventually many). Instrumenting
code that is still changing shape would be wasted effort.

## Consequences

### Positive

- Avoids instrumenting a moving target; keeps the early stages lean.

### Negative

- No visibility into VM behavior in the meantime — acceptable while naos runs a
  single interactive VM.

### Neutral

- Commits, directionally, to rondo as the observability path; the exact
  integration surface is decided at acceptance.

## Alternatives Considered

To be evaluated at acceptance: rondo (chosen direction), an
OpenTelemetry/Prometheus exposition endpoint, or structured logs only.

**Blocking dependencies:** [[0007-management-and-control-api]]; a
persistent/fleet workload actually worth observing.

## References

- [[0007-management-and-control-api]]
- [[0002-microvm-first-incremental-milestone-ladder]]
