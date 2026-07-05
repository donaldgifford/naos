---
id: DESIGN-0003
title: "M4 — guest networking and SSH"
status: Draft
author: Donald Gifford
created: 2026-07-05
---
<!-- markdownlint-disable-file MD025 MD041 -->

# DESIGN 0003: M4 — guest networking and SSH

**Status:** Draft
**Author:** Donald Gifford
**Date:** 2026-07-05

<!--toc:start-->
- [Overview](#overview)
- [Goals and Non-Goals](#goals-and-non-goals)
  - [Goals](#goals)
  - [Non-Goals](#non-goals)
- [Background](#background)
- [Detailed Design](#detailed-design)
  - [Component layout](#component-layout)
  - [The virtio-net device](#the-virtio-net-device)
  - [Data path: RX (host → guest)](#data-path-rx-host--guest)
  - [Data path: TX (guest → host)](#data-path-tx-guest--host)
  - [The host tap device](#the-host-tap-device)
  - [Host connectivity](#host-connectivity)
  - [Guest side](#guest-side)
  - [SSH — the success criterion](#ssh--the-success-criterion)
- [API / Interface Changes](#api--interface-changes)
- [Data Model](#data-model)
- [Testing Strategy](#testing-strategy)
- [Migration / Rollout Plan](#migration--rollout-plan)
- [Open Questions](#open-questions)
  - [1. RX backpressure with no posted buffers](#1-rx-backpressure-with-no-posted-buffers)
  - [2. virtio-mmio layout for the net device](#2-virtio-mmio-layout-for-the-net-device)
  - [3. virtio-queue and virtio-device API specifics](#3-virtio-queue-and-virtio-device-api-specifics)
  - [4. MAC address assignment](#4-mac-address-assignment)
  - [5. DHCP versus static as the default](#5-dhcp-versus-static-as-the-default)
  - [6. Where connectivity setup lives](#6-where-connectivity-setup-lives)
  - [7. CI capability for the SSH test](#7-ci-capability-for-the-ssh-test)
- [References](#references)
<!--toc:end-->

## Overview

M4 gives the guest a network interface and delivers SSH access into the VM. It
adds a **virtio-net** device on the guest side and a host **tap** device on the
VMM side, wired together over the same virtio-mmio + virtqueue + event-loop
foundation built for block storage in [[0002-m3-block-storage-via-virtio-blk]].
The milestone succeeds when a user runs `ssh user@<guest-ip>` from the host,
executes commands in the guest, and exits cleanly — the first time naos runs a
service a human interacts with over a real network.

## Goals and Non-Goals

### Goals

- Implement a **virtio-net** device over the existing virtio-mmio transport: a
  receive (RX) virtqueue, a transmit (TX) virtqueue, and the 12-byte
  virtio-net header on every frame.
- Move Ethernet frames between the guest's virtqueues and a host **tap** fd,
  driven by the epoll event loop: tap-readable feeds the guest RX queue; guest
  TX notifications drain the TX queue to the tap.
- Deliver RX interrupts to the guest via **irqfd** and receive TX notifications
  via **ioeventfd**, with no userspace vmexit round-trip on the fast path.
- Provide host connectivity — a default point-to-point `/30` plus NAT — so the
  guest reaches the host and the LAN, with bridging documented as the
  alternative.
- Ship a guest rootfs with `CONFIG_VIRTIO_NET`, an `sshd`, host keys, a login
  user, and a minimal network-config init step.
- Add `--net` / `--tap <name>` CLI flags; keep M2 and M3 working with no
  networking configured.

### Non-Goals

- **Multiple NICs, multiqueue, or vhost-net offload.** One virtio-net device,
  one RX/TX pair, backend in the VMM's own I/O thread. `vhost-net` and mergeable
  RX buffers are later performance work.
- **The virtio-net control queue (VIRTQ 2).** Deferred; see Detailed Design. We
  advertise a fixed MAC and no offloads, so no control-plane negotiation is
  needed for M4.
- **Checksum/TSO/GSO/UFO offloads and multicast filtering.** Not negotiated.
- **DHCP server ownership, IPAM, or overlay networking.** Host-side addressing
  is a static default with an optional `dnsmasq`; fleet-scale IPAM is out of
  scope.
- **The jailer / unprivileged operation.** tap requires elevated privilege; M4
  runs unsandboxed and names that gap explicitly. Confining the VMM is
  [[0010-guest-isolation-jailer]], deferred.
- **User-mode networking (passt/slirp).** A rootless alternative recorded in
  [[0006-guest-networking-via-virtio-net-and-tap]], not built here.

## Background

[[0006-guest-networking-via-virtio-net-and-tap]] decided the shape: virtio-net
in the guest and a host tap device, the Firecracker / Cloud Hypervisor model
and the most direct path to `ssh user@<vm-ip>`. This design implements that
decision.

M4 is the second virtio device, and it is deliberately sequenced after block
storage so that the hard, shared plumbing is already paid for. From
[[0002-m3-block-storage-via-virtio-blk]] and its ADRs we inherit:

- **virtio-mmio transport** ([[0004-virtio-over-mmio-device-transport]]): each
  device occupies a fixed MMIO region and a fixed IRQ line, discovered by the
  guest through a `virtio_mmio.device=<size>@<addr>:<irq>` kernel-cmdline token
  rather than PCI enumeration. An MMIO dispatch ("bus") routes guest MMIO
  vmexits to the right device.
- **Virtqueues** via rust-vmm's `virtio-queue`: the split-virtqueue descriptor
  table, available ring, and used ring, laid out in guest memory and accessed
  through `vm-memory`.
- **The epoll event loop** ([[0003-event-driven-epoll-concurrency-model]]): the
  vCPU runs on its own thread blocking in `KVM_RUN`; host-side fds are
  registered with an `event-manager` epoll loop on a separate thread and
  serviced on readiness. **irqfd** injects device interrupts without a
  userspace round-trip; **ioeventfd** turns a guest notification write into an
  eventfd signal instead of a vmexit.

Networking reuses all of it. A virtio-net device is, mechanically, "two
virtqueues and a file descriptor" bolted onto that foundation. The genuinely
new work is the backend that shuttles frames between the queues and the tap,
the tap device itself, and the host/guest network configuration around it.

The device protocol follows the **virtio 1.2 specification**, §5.1
("Network Device"). The host tap uses the Linux **TUN/TAP** interface
(`/dev/net/tun`), and the guest needs `CONFIG_VIRTIO_NET` in its kernel.

## Detailed Design

### Component layout

```text
        host I/O thread (epoll)                      vCPU thread
  ┌───────────────────────────────────┐        ┌───────────────────┐
  │  event-manager epoll loop         │        │   KVM_RUN loop     │
  │                                   │        │                    │
  │  ┌── tap fd (readable) ───────┐   │        │  guest executes    │
  │  │      RX path               │   │        │  virtio-net driver │
  │  │  read frame from tap  ─────┼───┼──┐     │                    │
  │  └────────────────────────────┘   │  │     │  TX notify write ──┼──┐
  │  ┌── tx ioeventfd (signalled) ┐   │  │     └───────────────────┘  │
  │  │      TX path               │◄──┼──┼──────────ioeventfd─────────┘
  │  │  drain TX q → write to tap │   │  │
  │  └────────────────────────────┘   │  │  RX: fill descriptor,
  │                                   │  │      write used ring,
  │  net::Backend                     │  │      pulse rx irqfd ──────┐
  │  ┌─────────────────────────────┐  │  │                          │
  │  │ RX vq (0) │ TX vq (1)        │◄─┼──┘                          ▼
  │  └─────────────────────────────┘  │                 KVM injects device IRQ
  └───────────────────────────────────┘                 into the guest vCPU
                  │
                  ▼
        /dev/net/tun  →  naos-tap0  →  (NAT / bridge)  →  host + LAN
```

The backend lives entirely on the I/O thread. The vCPU thread never touches
the tap or the queues directly; it only executes guest code and, when the guest
kicks the TX queue, that MMIO write is intercepted in-kernel by an ioeventfd and
turned into an eventfd wake for the I/O thread. Interrupts flow the other way
through irqfd. This keeps the two threads sharing only guest memory (already
shared for M3) and a pair of eventfds per queue.

### The virtio-net device

virtio-net exposes two virtqueues for the data plane:

- **Queue 0 — receive (RX).** The guest driver *pre-posts* empty buffers here
  for the device to fill with inbound frames. The available ring holds buffers
  the guest has offered; the device fills one and moves it to the used ring.
- **Queue 1 — transmit (TX).** The guest posts outbound frames here. The
  available ring holds frames the guest wants sent; the device consumes them
  and moves the (now-free) descriptor to the used ring.

The optional **control queue (queue 2)** carries out-of-band commands — MAC
programming, RX-mode/promisc changes, offload toggles, VLAN filters. M4 does
**not** advertise `VIRTIO_NET_F_CTRL_VQ`, so the guest never creates it; we
present a fixed MAC and a fixed feature set instead. It is called out here
because it is the natural next increment.

**The virtio-net header.** Every frame on both queues is prefixed by a
`struct virtio_net_hdr` (virtio 1.2 §5.1.6). With modern virtio 1.x and no
mergeable-RX-buffer negotiation, the header is **12 bytes**: `flags`,
`gso_type`, `hdr_len`, `gso_size`, `csum_start`, `csum_offset`, and a 2-byte
`num_buffers`. Because we negotiate no checksum or GSO offloads, the backend
writes an all-zero header on RX and ignores the header fields on TX (beyond
skipping past them to find the Ethernet frame). Getting the header length and
placement exactly right is the single most common virtio-net bug, so it gets
its own constant and its own unit test.

**Feature negotiation.** The device advertises the mandatory modern-virtio bits
(`VIRTIO_F_VERSION_1`) plus `VIRTIO_NET_F_MAC` (so the guest reads a stable MAC
from config space). It deliberately does **not** advertise checksum/TSO/GSO,
mergeable RX buffers, the control queue, or multiqueue for M4. Fewer negotiated
features means a simpler, more auditable backend; each can be added later behind
its own feature bit.

**Device config space.** The virtio-mmio device config region exposes the
6-byte MAC address (and, unused by us, `status`/`max_virtqueue_pairs`). The MAC
is assigned by the VMM — a locally-administered address derived from the tap
name or passed on the CLI — so the guest interface comes up with a deterministic
hardware address.

We build the device on rust-vmm's `virtio-device` (device-type/state helpers,
feature and config plumbing) and `virtio-queue` (the split-virtqueue rings over
`vm-memory`), the same crates M3 introduced. No new virtio dependency is
required.

### Data path: RX (host → guest)

1. The tap fd becomes readable; epoll wakes the I/O thread.
2. The backend reads one Ethernet frame from the tap.
3. It pops a descriptor chain from the **RX** available ring. If the guest has
   posted no RX buffers, the frame is dropped (or the read deferred) — a normal,
   transient condition under load.
4. It writes a 12-byte zero `virtio_net_hdr` followed by the frame bytes into
   the guest buffer via `vm-memory`, then places the chain on the used ring with
   the total written length.
5. It pulses the RX **irqfd**. KVM injects the device's IRQ line; the guest's
   virtio-net driver wakes, reads the used ring, and hands the frame up its
   network stack.

### Data path: TX (guest → host)

1. The guest driver enqueues a frame on the **TX** available ring and writes the
   virtio-mmio `QueueNotify` register.
2. That write matches a registered **ioeventfd**, so KVM signals an eventfd
   in-kernel instead of exiting to the vCPU userspace handler. The vCPU re-enters
   the guest immediately; the I/O thread's epoll observes the eventfd.
3. The backend drains the TX available ring: for each descriptor chain it skips
   the 12-byte header, gathers the Ethernet frame from guest memory, and
   `write()`s it to the tap fd.
4. It returns each consumed chain to the used ring and pulses the TX **irqfd** so
   the guest can reclaim the buffers.

Both paths are bounded per wake (process at most a queue's worth of buffers,
then yield back to epoll) so neither the tap nor a busy guest can starve the
other fd sources on the loop.

### The host tap device

The backend opens `/dev/net/tun` and issues `TUNSETIFF` with flags
`IFF_TAP | IFF_NO_PI`:

- `IFF_TAP` selects a layer-2 tap (full Ethernet frames) rather than a layer-3
  tun (IP packets only). virtio-net is an Ethernet NIC, so we want frames.
- `IFF_NO_PI` suppresses the 4-byte packet-information prefix the kernel would
  otherwise prepend, so what we read from the fd is exactly the Ethernet frame —
  no extra header to strip beyond the virtio-net header on the queue side.

The interface is named `naos-tap0` by default (overridable via `--tap`). The tap
fd is set non-blocking and registered with the epoll loop for read readiness; it
also serves as the write sink for TX. `vmm-sys-util` provides the fd and ioctl
helpers; the `TUNSETIFF` call is a thin `ioctl` wrapper over the raw
`ifreq`/flags, isolated to the tap module the way `serial.rs` isolates its
device.

**Privilege.** Creating or configuring a tap requires `CAP_NET_ADMIN`. There are
two operational modes:

- **VMM creates the tap.** Simplest to run, but naos needs `CAP_NET_ADMIN`
  (root, `setcap`, or the capability granted) — broad privilege held by the VMM
  process for the VM's whole lifetime.
- **Pre-created tap.** An administrator (or a setup script / systemd unit)
  creates and configures `naos-tap0` up front and grants the VMM user access;
  naos merely *attaches* to the existing interface by name. This keeps the VMM
  itself unprivileged at run time.

Either way, tap is a real privilege and a real host-network attack surface. This
is exactly the concern [[0010-guest-isolation-jailer]] tracks: once the VMM
holds `CAP_NET_ADMIN` and bridges a guest onto the host network, a jailer
(seccomp, namespaces, chroot, capability dropping) becomes load-bearing rather
than optional. M4 ships without it and states the gap plainly; the pre-created
tap mode is the interim mitigation.

### Host connectivity

A tap by itself is an isolated stub; the host must route between the tap and the
rest of the world. Two standard options, and naos documents both with a concrete
default:

- **Default: point-to-point `/30` + NAT.** Give `naos-tap0` a host-side address
  (e.g. `10.0.15.1/30`) and the guest the peer address (`10.0.15.2/30`). Enable
  IPv4 forwarding and add an `nftables` (or legacy `iptables`) masquerade rule on
  the host's uplink so guest traffic is source-NATed out. This needs no bridge,
  does not disturb the host's existing LAN, and is the least surprising default
  for a single microVM on a developer's machine. The guest reaches the host at
  `10.0.15.1` and the LAN/internet via NAT; the host reaches the guest (and its
  sshd) at `10.0.15.2`.
- **Alternative: bridge.** Enslave `naos-tap0` into a Linux bridge that also
  carries the host uplink (or a dedicated VM bridge). The guest then sits on the
  same L2 segment as the host/LAN and can take a LAN IP directly. This is the
  right choice for multiple VMs that must talk to each other or be reachable from
  other LAN hosts; it is more intrusive to host networking, so it is the opt-in
  rather than the default.

Guest addressing is either **static** (passed on the kernel cmdline / guest
config and applied by the init step) or **DHCP** (a host-side `dnsmasq` bound to
`naos-tap0` leasing the `/30` or bridge subnet). The M4 default is static
addressing on the `/30`: fewer moving parts, deterministic guest IP, and nothing
to fail before sshd comes up.

The host-side setup (tap creation, addressing, forwarding, NAT rule) is
inherently environment-specific and privileged. It is scripted in a
`scripts/`/`just` recipe rather than performed by the VMM, keeping the VMM's job
to "open/attach a tap fd and move frames."

### Guest side

The guest image gains four things:

- **`CONFIG_VIRTIO_NET`** in the kernel (alongside the `CONFIG_VIRTIO_MMIO` the
  M3 kernel already carries). The `virtio_mmio.device=` cmdline token for the
  net device makes the driver bind without PCI probing.
- **An SSH server.** `dropbear` for a minimal microVM image (tiny, single
  binary) or `openssh-server` for a fuller distro rootfs. Either satisfies the
  success criterion; dropbear is the default for the small image.
- **Host keys and a login user.** Generated into the rootfs at build time: SSH
  host keys plus one user authenticating by public key (preferred) or password.
- **A network-config init step.** A few lines in the init sequence: bring the
  interface up and assign the address — `ip addr add 10.0.15.2/30 dev eth0` +
  `ip route add default via 10.0.15.1`, or `udhcpc -i eth0` if using DHCP —
  before `sshd` starts. Small enough to live in the existing init script.

### SSH — the success criterion

With the tap up, the host route/NAT in place, the guest addressed, and sshd
running, the milestone is met by:

```text
ssh user@10.0.15.2      # (or the DHCP/bridge-assigned address)
```

connecting from the host, running commands in the guest, and exiting cleanly.
That end-to-end path — host TCP → tap → RX queue → guest sshd, and the reverse
for responses — exercises every piece M4 adds. **`ssh user@<guest-ip>`, run
commands, exit** is the M4 success criterion.

## API / Interface Changes

New CLI flags on the `naos-linux` `Args` (clap), additive to the existing
`--kernel` / `--mem` / `--cmdline`:

- `--net` — enable guest networking. When absent, no virtio-net device is
  created and the VM behaves exactly as in M2/M3. Networking is strictly opt-in.
- `--tap <name>` — the tap interface to open or attach to. Defaults to
  `naos-tap0`. Implies `--net`.
- `--mac <addr>` (optional) — override the guest MAC. Defaults to a
  locally-administered address derived from the tap name, so runs are
  deterministic without the flag.

The kernel `--cmdline` gains a second `virtio_mmio.device=<size>@<addr>:<irq>`
token for the net device's MMIO region and IRQ when `--net` is set, appended by
the VMM rather than hand-written by the user.

No changes to the M2/M3 interface: with none of the above flags, existing
invocations are byte-for-byte unchanged and no tap is opened.

## Data Model

**Guest physical layout.** The M3 MMIO device region gains a second, adjacent
virtio-mmio window for the net device, each a fixed size with its own IRQ line,
both above the guest RAM top and below the legacy high-MMIO area — the fixed,
hardcoded layout [[0004-virtio-over-mmio-device-transport]] commits to. The
exact base/size/IRQ are pinned as constants next to the block device's, in the
MMIO-bus module.

**Virtqueues (per queue, in guest RAM, via `virtio-queue`).** Standard
split-virtqueue layout: a descriptor table, an available ring, and a used ring,
sized by the queue length the VMM advertises (a power of two, e.g. 256). The RX
and TX queues are independent instances of the same structure.

**Frame buffer format (on the wire between guest and backend).**

```text
┌───────────────────────────┬─────────────────────────────────────────┐
│ virtio_net_hdr (12 bytes) │ Ethernet frame (dst/src MAC, type, ...)  │
│ flags, gso_type, hdr_len, │ up to guest MTU + L2 header              │
│ gso_size, csum_start,     │                                          │
│ csum_offset, num_buffers  │                                          │
└───────────────────────────┴─────────────────────────────────────────┘
   RX: written zeroed by backend        RX: frame read from tap
   TX: written by guest, skipped        TX: frame written to tap fd
```

**Device config space (virtio-mmio config region).** 6-byte MAC address exposed
read-only to the guest; other virtio-net config fields (`status`,
`max_virtqueue_pairs`) present but unused because their feature bits are not
advertised.

No persistent/on-disk schema: virtio-net state is entirely in-memory (queues in
guest RAM, backend state on the I/O thread). Persistence remains the block
device's concern.

## Testing Strategy

**Unit tests (host-only, no KVM required):**

- **virtio-net header.** Length is exactly 12 bytes; RX path writes an all-zero
  header ahead of the frame; TX path skips exactly the header and recovers the
  original Ethernet bytes. This is the highest-value unit test — header
  mishandling is the classic virtio-net failure.
- **Backend queue handling.** With a `vm-memory` guest-memory fixture and
  hand-built `virtio-queue` rings, drive RX (post buffers → deliver frame →
  assert used ring + written length) and TX (post frame → assert bytes handed to
  a fake tap sink). No real tap needed: the tap is abstracted behind a
  read/write trait so a `Vec`-backed fake stands in.
- **Feature negotiation and config space.** Advertised feature bits match the
  intended set; the MAC read back from config space matches what was assigned.
- **CLI parsing.** `--net` / `--tap` / `--mac` defaults and overrides, matching
  the existing `main.rs` clap tests; `--net` absent creates no device.

**Integration tests (require `/dev/kvm` and `CAP_NET_ADMIN`, skip cleanly
without, mirroring the existing gated vCPU tests):**

- **tap loopback.** Open a tap, write a frame in, assert the backend delivers it
  to the RX queue; enqueue a frame on TX, assert it appears on the tap.
- **End-to-end SSH.** Boot the guest image with `--net`, wait for the guest IP
  to answer, run `ssh user@<guest-ip> 'uname -a'` from the test, assert the
  command output and a zero exit. This is the milestone's acceptance test; it is
  the one test that proves M4, so it runs in CI on a KVM-capable, privileged
  runner (or is documented as a manual `just` recipe where CI cannot grant the
  capability).

**Manual smoke test:** `just run --net --kernel … --disk …`, then
`ssh user@10.0.15.2` from another terminal, `ls`, `ip addr`, exit.

## Migration / Rollout Plan

Networking is additive and opt-in, so M2/M3 users are unaffected until they pass
`--net`. The work lands in reviewable increments, each independently testable:

1. **Tap module.** Open `/dev/net/tun`, `TUNSETIFF(IFF_TAP|IFF_NO_PI)`,
   non-blocking fd, name `naos-tap0`. Unit-test the ioctl wrapper; smoke-test by
   reading/writing frames from a shell (`ip`/`ping` against a manually addressed
   tap).
2. **virtio-net device + backend.** RX/TX queues on the existing MMIO bus, the
   12-byte header, feature/config plumbing, and the frame-shuttling backend
   against a *fake* tap sink. Fully unit-tested with no KVM.
3. **Event-loop wiring.** Register the tap fd for read, TX ioeventfd for notify,
   RX/TX irqfds for interrupts — reusing the M3 registration path. Boot a guest
   and confirm the interface appears (`ip link`) and ARP/ping to the host works.
4. **Host connectivity script.** The `/30` + NAT default as a `just`/`scripts`
   recipe (tap addressing, `net.ipv4.ip_forward`, nftables masquerade), with the
   bridge variant documented alongside.
5. **Guest image.** Add `CONFIG_VIRTIO_NET`, dropbear/openssh, host keys, a
   user, and the network-config init step to the rootfs build.
6. **SSH acceptance.** `--net` end to end; `ssh user@<guest-ip>` runs a command
   and exits. Milestone met.

Rollback is trivial at every step: omitting `--net` disables the entire feature
path. The known accepted risk carried forward is the tap privilege / unsandboxed
VMM, tracked by [[0010-guest-isolation-jailer]] and mitigated in the interim by
the pre-created-tap mode.

## Open Questions

Each item is a decision to settle before this design moves from Draft to
Approved. Option **a** is the recommendation; **b** onward are alternatives;
**other** is a write-in. Record the choice on the **Decision** line.

### 1. RX backpressure with no posted buffers

- **a (recommended).** Stop reading the tap (leave it readable) until the guest
  posts RX buffers, relying on TCP retransmit — gentler on the guest, no frame
  loss. Validate against a real `iperf`/`ssh` workload.
- **b.** Drop the frame — simplest, and what real hardware does on RX overflow.
- **other.** *(write-in)*

**Decision:** *pending*

### 2. virtio-mmio layout for the net device

- **a (recommended).** Allocate the net device the next slot after the block
  device in the M3 MMIO/GSI layout; confirm no collision with M3 or the guest
  kernel's expectations before wiring.
- **b.** Carve out a separate reserved MMIO/GSI region for network devices.
- **other.** *(write-in)*

**Decision:** *pending*

### 3. virtio-queue and virtio-device API specifics

- **a (recommended).** Confirm the exact types (descriptor-chain iteration,
  used-ring signalling, config space) against `docs.rs` for the pinned crate
  versions when coding, rather than assuming a shape now.
- **b.** Hand-roll queue handling over the raw descriptor types.
- **other.** *(write-in)*

**Decision:** *pending*

### 4. MAC address assignment

- **a (recommended).** Derive a deterministic locally-administered MAC from the
  tap name.
- **b.** Require an explicit `--mac`.
- **other.** *(write-in)*

**Decision:** *pending*

### 5. DHCP versus static as the default

- **a (recommended).** Ship a static `/30` point-to-point for determinism now;
  add DHCP (host `dnsmasq`) alongside the bridge path later.
- **b.** DHCP via a host `dnsmasq` from the start — friendlier for multiple or
  bridged VMs.
- **other.** *(write-in)*

**Decision:** *pending*

### 6. Where connectivity setup lives

- **a (recommended).** A `just` recipe plus a `scripts/` helper for M4; defer
  moving tap/NAT setup into a privileged VMM phase (or the future jailer,
  [[0010-guest-isolation-jailer]]).
- **b.** Build tap/NAT setup into naos itself as a privileged setup phase now.
- **other.** *(write-in)*

**Decision:** *pending*

### 7. CI capability for the SSH test

- **a (recommended).** Keep the SSH end-to-end test a documented manual gate
  (like the KVM-gated tests) unless runners can grant `CAP_NET_ADMIN`.
- **b.** Run it in CI on a privileged or self-hosted runner that grants
  `CAP_NET_ADMIN`.
- **other.** *(write-in)*

**Decision:** *pending*

## References

- [[0006-guest-networking-via-virtio-net-and-tap]] — the ADR this design
  implements.
- [[0004-virtio-over-mmio-device-transport]] — virtio-mmio transport and MMIO
  bus.
- [[0003-event-driven-epoll-concurrency-model]] — epoll loop, irqfd, ioeventfd.
- [[0002-microvm-first-incremental-milestone-ladder]] — where M4 sits and its
  success criterion.
- [[0002-m3-block-storage-via-virtio-blk]] — the virtio-mmio + virtqueue +
  event-loop foundation M4 reuses.
- [[0001-m2-interactive-serial-console]] — the event loop and interactive-access
  baseline.
- [[0010-guest-isolation-jailer]] — the deferred jailer that will confine the
  `CAP_NET_ADMIN`-holding VMM.
- **virtio 1.2 specification**, §5.1 "Network Device" — RX/TX virtqueues, the
  optional control queue, and `struct virtio_net_hdr` (the 12-byte header,
  §5.1.6): <https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html>
- **Linux TUN/TAP** — `Documentation/networking/tuntap.rst`: `/dev/net/tun`,
  `TUNSETIFF`, `IFF_TAP`, `IFF_NO_PI`:
  <https://docs.kernel.org/networking/tuntap.html>
- **`CONFIG_VIRTIO_NET`** — guest virtio-net driver (`drivers/net/virtio_net.c`).
- **rust-vmm crates**: [`virtio-device`](https://docs.rs/virtio-device),
  [`virtio-queue`](https://docs.rs/virtio-queue),
  [`vm-memory`](https://docs.rs/vm-memory),
  [`vmm-sys-util`](https://docs.rs/vmm-sys-util),
  [`kvm-ioctls`](https://docs.rs/kvm-ioctls) — irqfd (`register_irqfd`) and
  ioeventfd (`register_ioevent`).
- **KVM API** — `KVM_IRQFD`, `KVM_IOEVENTFD`:
  <https://docs.kernel.org/virt/kvm/api.html>
