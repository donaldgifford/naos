# Architecture Decision Records (ADRs)

This directory contains Architecture Decision Records documenting significant
technical decisions.

## What are ADRs?

ADRs document **technical implementation decisions** for specific architectural
components. Each ADR focuses on a single decision and includes:

- **Context**: The problem or constraint that led to this decision
- **Decision**: What was chosen and why
- **Consequences**: Trade-offs, pros, and cons
- **Alternatives**: Other options that were considered

## Creating a New ADR

```bash
docz create adr "Your ADR Title"
```

## ADR Status

- **Proposed**: Under discussion, not yet approved
- **Accepted**: Approved and being implemented or already implemented
- **Deprecated**: No longer relevant or superseded
- **Superseded by ADR-XXXX**: Replaced by another ADR

<!-- BEGIN DOCZ AUTO-GENERATED -->
## All ADRs

| ID | Title | Status | Date | Author | Link |
|----|-------|--------|------|--------|------|
| ADR-0001 | naos is one platform: learning project and homelab VMM | Accepted | 2026-07-05 | Donald Gifford | [0001-naos-is-one-platform-learning-and-homelab.md](0001-naos-is-one-platform-learning-and-homelab.md) |
| ADR-0002 | microVM-first, with an incremental milestone ladder | Accepted | 2026-07-05 | Donald Gifford | [0002-microvm-first-incremental-milestone-ladder.md](0002-microvm-first-incremental-milestone-ladder.md) |
| ADR-0003 | Event-driven (epoll) concurrency model | Accepted | 2026-07-05 | Donald Gifford | [0003-event-driven-epoll-concurrency-model.md](0003-event-driven-epoll-concurrency-model.md) |
| ADR-0004 | virtio over MMIO as the device transport | Accepted | 2026-07-05 | Donald Gifford | [0004-virtio-over-mmio-device-transport.md](0004-virtio-over-mmio-device-transport.md) |
| ADR-0005 | Root filesystem: initramfs first, then virtio-blk | Accepted | 2026-07-05 | Donald Gifford | [0005-root-filesystem-initramfs-then-virtio-blk.md](0005-root-filesystem-initramfs-then-virtio-blk.md) |
| ADR-0006 | Guest networking via virtio-net and a host tap device | Accepted | 2026-07-05 | Donald Gifford | [0006-guest-networking-via-virtio-net-and-tap.md](0006-guest-networking-via-virtio-net-and-tap.md) |
| ADR-0007 | Management and control API | Proposed | 2026-07-05 | Donald Gifford | [0007-management-and-control-api.md](0007-management-and-control-api.md) |
| ADR-0008 | Observability and metrics via rondo | Proposed | 2026-07-05 | Donald Gifford | [0008-observability-and-metrics-via-rondo.md](0008-observability-and-metrics-via-rondo.md) |
| ADR-0009 | naos-vmm hypervisor abstraction layer | Proposed | 2026-07-05 | Donald Gifford | [0009-naos-vmm-hypervisor-abstraction-layer.md](0009-naos-vmm-hypervisor-abstraction-layer.md) |
| ADR-0010 | Guest isolation via a jailer | Proposed | 2026-07-05 | Donald Gifford | [0010-guest-isolation-jailer.md](0010-guest-isolation-jailer.md) |
<!-- END DOCZ AUTO-GENERATED -->
