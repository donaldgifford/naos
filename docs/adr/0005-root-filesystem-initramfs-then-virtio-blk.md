---
id: ADR-0005
title: "Root filesystem: initramfs first, then virtio-blk"
status: Accepted
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# 0005. Root filesystem: initramfs first, then virtio-blk

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

To run userspace — a shell first, services later — the guest needs a root
filesystem. There are two mechanisms:

1. An **initramfs** loaded into guest RAM by the VMM (no device at all), which
   the kernel unpacks as its rootfs.
2. A **block device** (virtio-blk) backed by a disk image.

We need to pick the near-term rootfs strategy for M2 and M3.

## Decision

Use an **initramfs first (M2), then add virtio-blk (M3)**:

- **M2:** load a small busybox initramfs into guest memory (like the kernel
  itself), and point the kernel at it via `boot_params`. This yields an
  interactive rootfs with zero device machinery, isolating the event-loop change
  ([[0003-event-driven-epoll-concurrency-model]]) from any virtio work.
- **M3:** add virtio-blk backed by a raw image for a persistent, larger rootfs
  (Alpine or minimal Debian).

Both remain first-class: initramfs is a legitimate fast-boot, RAM-only microVM
mode; virtio-blk is the path to persistent and general-purpose rootfs.

## Consequences

### Positive

- M2 gets a real userspace shell without any virtio implementation.
- The two mechanisms are complementary: RAM-rootfs microVM vs disk-backed VM.
- Keeps each milestone minimal.

### Negative

- Two rootfs paths to maintain.
- initramfs rootfs is ephemeral and RAM-limited.
- The guest kernel must gain initramfs support now and virtio-blk + a filesystem
  driver at M3.

### Neutral

- Introduces a boot-artifact story (how images and initramfs are built and
  referenced) that will grow over time.

## Alternatives Considered

- **virtio-blk only, skip initramfs.** Viable and less duplication, but couples
  the first interactive milestone to the full virtio stack — more to debug at
  once.
- **virtio-fs / 9p shared-directory rootfs.** Rejected for now: more complex and
  less representative of the microVM target.

## References

- [[0002-microvm-first-incremental-milestone-ladder]], [[0004-virtio-over-mmio-device-transport]]
- [[0001-m2-interactive-serial-console]], [[0002-m3-block-storage-via-virtio-blk]]
