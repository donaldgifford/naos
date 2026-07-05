---
id: ADR-0004
title: "virtio over MMIO as the device transport"
status: Accepted
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# 0004. virtio over MMIO as the device transport

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

Milestones M3 and beyond need paravirtual devices (block, then net). A virtio
device attaches to the guest through a *transport*: legacy port I/O, PCI, or
MMIO. We must choose the transport for naos's microVM device model before
building the first device.

## Decision

Use **virtio-mmio** (virtio 1.x, modern) as the device transport:

- Each device gets a fixed **MMIO region** and a dedicated **IRQ line**,
  discovered by the guest via the kernel command line (`virtio_mmio.device=…`)
  or a small devicetree/ACPI stub. No PCI bus enumeration.
- Build a small **MMIO dispatch ("bus")** that routes guest MMIO vmexits to the
  right device, and inject device interrupts through the in-kernel IRQ chip via
  **irqfd**.
- This matches Firecracker's microVM model: minimal, no PCI, fast boot. The same
  MMIO + virtqueue plumbing serves both virtio-blk (M3) and virtio-net (M4).

## Consequences

### Positive

- Far simpler than PCI: no config space, no bus enumeration, no BAR allocation.
- Minimal guest kernel config and a deterministic, hardcoded device layout.
- One transport implementation serves every future virtio device.

### Negative

- Devices are discovered by explicit cmdline/devicetree rather than probed —
  fine for our own images, awkward for arbitrary distro kernels that expect PCI.
- Hotplug is limited; a future general-VM path may eventually want PCI.

### Neutral

- Commits us to a fixed virtio-mmio address/IRQ layout, another deliberate
  hardcoded decision (per "opinions, not options").

## Alternatives Considered

- **virtio-pci.** Rejected for now: PCI complexity is unnecessary for microVMs;
  defer until a concrete general-VM use case demands it.
- **Legacy virtio port I/O.** Rejected: deprecated and more per-device wiring.

Likely building blocks: rust-vmm's `virtio-device` / `virtio-queue` crates.

## References

- [[0002-microvm-first-incremental-milestone-ladder]]
- [[0005-root-filesystem-initramfs-then-virtio-blk]], [[0006-guest-networking-via-virtio-net-and-tap]]
- [[0003-virtio-mmio-device-model]]
