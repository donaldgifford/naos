---
id: ADR-0002
title: "microVM-first, with an incremental milestone ladder"
status: Accepted
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# 0002. microVM-first, with an incremental milestone ladder

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

Given that naos is a real homelab platform ([[0001-naos-is-one-platform-learning-and-homelab]]),
we must choose what to build next and in what order. naos targets two workload
shapes:

1. **Purpose-fit microVMs** — small, fast-booting, minimal, run many at a time,
   automatable and manageable.
2. **General-purpose VMs** — stock Linux distributions (Debian, Ubuntu).

The MVP boots a kernel to a panic; it cannot yet run a workload. We need a build
order, and near-term milestones with concrete success criteria.

## Decision

**Build the microVM device substrate first.** General-purpose distro VMs run on
the *same* substrate — direct kernel boot plus virtio devices — so a general VM
is a bigger rootfs with more RAM/vCPUs, not a different architecture. (This is
how Firecracker runs stock Ubuntu.) microVM work therefore subsumes most
general-VM work; we do not have to choose between the two goals.

Sequence the Linux backend through an **incremental milestone ladder**, each
milestone a demoable artifact with a clear success criterion:

- **M2 — interactive serial console.** Event loop + serial input + an initramfs
  rootfs. *Success:* `just run` gives a shell prompt on the terminal; run
  `ls`, `uname -a`; `poweroff`; clean exit 0. Detailed in
  [[0001-m2-interactive-serial-console]].
- **M3 — block storage.** virtio-mmio transport + virtio-blk. *Success:* boot a
  persistent disk-image rootfs (Alpine or minimal Debian) to a login; write a
  file, reboot, it persists. Detailed in [[0002-m3-block-storage-via-virtio-blk]].
- **M4 — networking + SSH.** virtio-net + host tap. *Success:*
  `ssh user@<vm-ip>` from the host, run commands, exit. Detailed in
  [[0003-m4-guest-networking-and-ssh]].

Interactive access is **serial-console-first**: "log in and run commands and
exit" is delivered at M2 over the serial console. SSH is a *network* capability
(M4), not a prerequisite for logging in.

## Consequences

### Positive

- Each milestone is small, demoable, and de-risks the next.
- The virtio foundation built for block storage (M3) is reused by networking
  (M4).
- microVM-first delivers general-purpose distro VMs almost for free.

### Negative

- Full "manage many microVMs at scale / automation" capability (control API,
  config, jailer) is deliberately deferred until a single VM boots fully
  networked, so naos is not operationally manageable-at-scale for a while.

### Neutral

- SMP (multiple vCPUs), bzImage / distro-kernel boot, and UEFI/PCI are not on
  this ladder; they slot in later only if a concrete need arises.

## Alternatives Considered

- **General-VM-first** (boot stock Debian with its shipped bzImage, PCI, ACPI):
  rejected — more upfront complexity for the same device model microVMs already
  need.
- **SSH-first success criterion:** rejected — bundles block, networking, and
  sshd into one milestone, forcing three subsystems to be debugged at once.
- **Skip initramfs, go straight to virtio-blk for M2:** viable; rejected as the
  default because initramfs isolates the event-loop change and is itself a real
  microVM rootfs mode (see [[0005-root-filesystem-initramfs-then-virtio-blk]]).

## References

- [[0001-naos-is-one-platform-learning-and-homelab]]
- [[0003-event-driven-epoll-concurrency-model]], [[0004-virtio-over-mmio-device-transport]],
  [[0005-root-filesystem-initramfs-then-virtio-blk]], [[0006-guest-networking-via-virtio-net-and-tap]]
- `WALK-linux.md` §13 "What's next"
