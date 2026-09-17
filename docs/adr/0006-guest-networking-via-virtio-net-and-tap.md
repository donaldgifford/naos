---
id: ADR-0006
title: "Guest networking via virtio-net and a host tap device"
status: Accepted
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# 0006. Guest networking via virtio-net and a host tap device

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

SSH and any real workload need guest networking. Given the virtio-mmio transport
decision ([[0004-virtio-over-mmio-device-transport]]), the guest-side device
will be **virtio-net**. The open choice is the *host* side: how packets move
between the VM and the host/LAN — a tap device, user-mode networking
(slirp/passt), or macvtap/bridge.

## Decision

Use **virtio-net in the guest and a host tap device** for the M4 milestone, with
the host providing connectivity via a bridge or NAT:

- naos opens/attaches a **tap** fd and wires it to the virtio-net backend over
  the event loop.
- The guest gets an IP (static, or from the host), giving real L2/L3
  connectivity — enough for SSH and arbitrary services.
- This is the Firecracker / Cloud Hypervisor model and the most direct path to
  the M4 success criterion (`ssh user@<vm-ip>`).

## Consequences

### Positive

- Real, performant, well-understood networking that supports SSH and any
  service.
- Reuses the virtio-mmio + virtqueue foundation from M3.

### Negative

- tap requires host privileges (`CAP_NET_ADMIN`) or pre-created taps — a setup
  burden and a security surface that directly motivates the future jailer
  ([[0010-guest-isolation-jailer]]).
- Host-side routing/NAT/bridge configuration is fiddly and environment-specific.

### Neutral

- Commits to an L2 tap model; a user-mode option can be added later for
  unprivileged/rootless scenarios.

## Alternatives Considered

- **User-mode networking (passt/slirp).** Deferred: unprivileged and simpler
  host setup, but slower and less transparent. A good later addition for
  rootless microVMs.
- **macvtap / direct bridge attachment.** Folded into the "bridge or NAT" host
  configuration rather than treated as a separate transport.

## References

- [[0002-microvm-first-incremental-milestone-ladder]], [[0004-virtio-over-mmio-device-transport]]
- [[0010-guest-isolation-jailer]]
- [[0005-guest-networking-and-ssh]]
