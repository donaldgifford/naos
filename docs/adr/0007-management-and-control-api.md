---
id: ADR-0007
title: "Management and control API"
status: Proposed
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# 0007. Management and control API

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

> **Tracked, not yet accepted.** This records a known future need so it is not
> forgotten. It is deliberately *not* accepted because it depends on work that
> is not done yet (see "Blocking dependencies").

## Context

A stated naos goal is running purpose-fit microVMs "at scale, automatable and
manageable." The current model is one process, one VM, launched from CLI
arguments, exiting when the guest halts. Managing many VMs —
create/start/stop/inspect, defined by config, launched unattended — needs a
control surface: a VM definition format and/or an API (Firecracker exposes an
HTTP API over a Unix socket), possibly a long-running daemon.

## Decision

Record the intent to add a **management/control API and config-driven VM
definition**. Do not accept yet.

Accept once (a) a complete VM boots through M4, and (b) the process model is
decided — one-process-per-VM (Firecracker-style) vs a daemon managing many
(libvirt-style), which is an open question in `ARCHITECTURE.md`. Managing many
VMs only becomes a concrete pressure after a single VM works end to end.

## Consequences

### Positive

- Keeps the foundational stages free of premature API/daemon design.

### Negative

- Ergonomics and automation are absent until this lands; early operation stays
  manual (CLI args, one VM at a time).

### Neutral

- The choice here is entangled with the process-model decision and will likely
  arrive alongside it.

## Alternatives Considered

To be evaluated at acceptance: HTTP API over a Unix socket (Firecracker-style),
gRPC, a config-file + CLI surface only, or a long-running daemon (libvirt-style).

**Blocking dependencies:** M4 complete ([[0003-m4-guest-networking-and-ssh]]);
the process-model decision.

## References

- [[0002-microvm-first-incremental-milestone-ladder]]
- [[0008-observability-and-metrics-via-rondo]]
- `ARCHITECTURE.md` — "Open architectural questions" (process model, API surface)
