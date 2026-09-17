---
id: ADR-0010
title: "Guest isolation via a jailer"
status: Proposed
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# 0010. Guest isolation via a jailer

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

> **Tracked, not yet accepted.** Recorded so the security gap is explicit;
> blocked on the dependencies below.

## Context

Running guest kernels — especially less-trusted ones, and especially once host
tap networking ([[0006-guest-networking-via-virtio-net-and-tap]]) requires
elevated capabilities like `CAP_NET_ADMIN` — is a real security surface. A
production-grade platform needs a **jailer**: seccomp filtering, namespaces,
chroot, capability dropping, and cgroup limits (Firecracker ships exactly such a
jailer). Today naos runs as a normal user process with broad access.

## Decision

Record the intent to add a **jailer** that confines the VMM process. Do not
accept yet.

Accept when guest isolation becomes concretely required — i.e., when running
less-trusted workloads or operating a fleet — which also depends on networking
(the source of the privilege need) and, ideally, the process-model decision.

## Consequences

### Positive

- Naming the gap now prevents it from being silently assumed away.

### Negative

- Early operation runs unsandboxed. Fine for the author's own kernels on trusted
  hosts; **not** fine for untrusted guests — a known, accepted-for-now gap.

### Neutral

- The jailer's shape depends on the process model and the networking privilege
  surface, so it will likely arrive alongside those.

## Alternatives Considered

To be evaluated at acceptance: a Firecracker-style jailer binary, systemd
sandboxing, running the VMM inside a container, or minijail.

**Blocking dependencies:** [[0006-guest-networking-via-virtio-net-and-tap]]
(privilege surface); less-trusted / multi-tenant workloads.

## References

- [[0006-guest-networking-via-virtio-net-and-tap]]
- `ARCHITECTURE.md` — "Open architectural questions" (jailer and sandboxing)
