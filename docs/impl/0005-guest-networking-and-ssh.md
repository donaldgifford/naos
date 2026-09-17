---
id: IMPL-0005
title: "Guest networking and SSH"
status: Draft
author: Donald Gifford
created: 2026-07-06
---
<!-- markdownlint-disable-file MD025 MD041 -->

# IMPL 0005: Guest networking and SSH

**Status:** Draft
**Author:** Donald Gifford
**Date:** 2026-07-06

<!--toc:start-->
- [Objective](#objective)
- [Scope](#scope)
  - [In Scope](#in-scope)
  - [Out of Scope](#out-of-scope)
- [Current State](#current-state)
- [Dependencies](#dependencies)
- [Implementation Phases](#implementation-phases)
  - [Phase 1: The virtio-net device and CLI flags](#phase-1-the-virtio-net-device-and-cli-flags)
    - [Tasks](#tasks)
    - [Success Criteria](#success-criteria)
  - [Phase 2: Tap backend and the frame data path](#phase-2-tap-backend-and-the-frame-data-path)
    - [Tasks](#tasks-1)
    - [Success Criteria](#success-criteria-1)
  - [Phase 3: Host connectivity and guest addressing](#phase-3-host-connectivity-and-guest-addressing)
    - [Tasks](#tasks-2)
    - [Success Criteria](#success-criteria-2)
  - [Phase 4: Guest rootfs with sshd](#phase-4-guest-rootfs-with-sshd)
    - [Tasks](#tasks-3)
    - [Success Criteria](#success-criteria-3)
  - [Phase 5: Frame-path tests and the SSH acceptance gate](#phase-5-frame-path-tests-and-the-ssh-acceptance-gate)
    - [Tasks](#tasks-4)
    - [Success Criteria](#success-criteria-4)
- [Open Questions](#open-questions)
  - [1. Tap open and attach semantics](#1-tap-open-and-attach-semantics)
  - [2. Defer and re-arm against the event loop](#2-defer-and-re-arm-against-the-event-loop)
  - [3. The MAC derivation function](#3-the-mac-derivation-function)
  - [4. Scripting the manual SSH gate](#4-scripting-the-manual-ssh-gate)
- [File Changes](#file-changes)
- [Testing Plan](#testing-plan)
- [References](#references)
<!--toc:end-->

## Objective

This implementation gives the guest a network interface and delivers SSH access
into the VM. It adds a virtio-net device over the virtio-mmio transport — an RX
and a TX virtqueue with the 12-byte virtio-net header — plus a host tap backend
that shuttles Ethernet frames between the guest's queues and a `/dev/net/tun` fd
driven by the event loop. With a static `/30` point-to-point link and NAT set up
by an external `just` recipe, and a guest rootfs running `sshd`, the milestone is
met when `ssh user@<guest-ip>` works from the host.

**Implements:** [[0005-guest-networking-and-ssh]]

## Scope

### In Scope

- A virtio-net device: an RX virtqueue (queue 0), a TX virtqueue (queue 1), the
  12-byte `virtio_net_hdr` on every frame, a 6-byte MAC in config space, and a
  lean feature set (`VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC`).
- A tap backend: open or attach `/dev/net/tun` with `IFF_TAP | IFF_NO_PI`,
  non-blocking fd, RX with defer-and-re-arm backpressure, TX drain to the tap.
- Event-loop wiring: the tap fd registered for read readiness, the TX
  `ioeventfd` doorbell, and the RX/TX `irqfd` interrupt path (reusing IMPL-0001
  and IMPL-0003).
- CLI: `--net`, `--tap <name>`, `--mac <addr>`; a `virtio_mmio.device=` token and
  an `ip=` token appended to the cmdline by the VMM when `--net` is set.
- A deterministic locally-administered MAC derived from the tap name, overridable
  with `--mac`.
- Host connectivity: a `scripts/` helper plus a `just` recipe that create the
  tap, assign the `/30`, enable forwarding, and install a NAT rule.
- A guest rootfs with `CONFIG_VIRTIO_NET`, an SSH server, host keys, a login
  user, and a network-config init step.
- Frame-path unit tests plus the SSH end-to-end as a documented manual /
  self-hosted gate.

### Out of Scope

- Multiple NICs, multiqueue, and `vhost-net` offload — one RX/TX pair, backend in
  the VMM's own I/O thread.
- The virtio-net control queue (queue 2); `VIRTIO_NET_F_CTRL_VQ` is not
  advertised, so the guest never creates it.
- Checksum, TSO, GSO, and UFO offloads, mergeable RX buffers, and multicast
  filtering — none negotiated.
- A DHCP server, IPAM, overlay networking, and the bridge topology as default
  (documented alongside, not built).
- The jailer and unprivileged operation ([[0010-guest-isolation-jailer]],
  deferred); the tap privilege gap is named, not closed.
- User-mode networking (passt/slirp), a rootless alternative recorded in the ADR.

## Current State

There is no networking today. `main.rs` parses only `--kernel`, `--mem`, and
`--cmdline` (default `console=ttyS0 reboot=k panic=1 pci=off`); there is no
`--net`. `boot::write_cmdline` and `boot::write_boot_params` assemble and write
that single cmdline into guest memory, and nothing appends device tokens.
`vmm.rs` `Vmm::new` builds guest RAM, loads the kernel, and creates only the
16550 serial device; `run()` is a single blocking `vcpu::run` loop with no I/O
thread. `vcpu.rs` handles PIO, `Hlt`, `Shutdown`, and the reset request, and
sends any `MMIO` exit to the defensive `bail!` arm — there is no `MmioBus`, no
`irqfd`, and no `ioeventfd`.

This IMPL builds on the virtio-mmio substrate that [[0003-virtio-mmio-device-model]]
delivers (the `MmioBus`, the modern virtio-mmio transport, the `VirtioDevice`
trait, the `Interrupt` handle, and the `virtio-queue` glue) and the event loop
that [[0001-event-loop-and-concurrency-model]] delivers (the I/O thread, the
`event-manager` epoll loop, `irqfd`, and `ioeventfd`). virtio-net is the second
device on that substrate, after block storage ([[0004-block-storage-via-virtio-blk]]);
mechanically it is "two virtqueues and a file descriptor."

## Dependencies

This work sits on two hard dependencies and cannot land before them:

- **[[0001-event-loop-and-concurrency-model]]** — the epoll I/O thread, `irqfd`
  for injecting RX/TX interrupts, `ioeventfd` for the TX doorbell, and the
  `event-manager` registration path the tap fd plugs into. Without the event loop
  there is nowhere to register a readable tap fd and no non-blocking data path.
- **[[0003-virtio-mmio-device-model]]** — the virtio-mmio transport, the
  `MmioBus`, the `VirtioDevice` trait, the `Interrupt` handle, and the
  `virtio-queue` split-virtqueue glue. virtio-net is implemented against exactly
  that trait and adds no transport code.

It also depends on:

- **TUN/TAP via `libc`.** The tap backend opens `/dev/net/tun` and issues
  `TUNSETIFF` with `IFF_TAP | IFF_NO_PI` through a thin `libc::ioctl` wrapper over
  `ifreq` — no large new crate. `vmm-sys-util` supplies `EventFd` and ioctl
  helpers, the way `serial.rs` already uses them.
- **`virtio-bindings`** (added by IMPL-0003) for `virtio_net_hdr` and
  `VIRTIO_NET_F_MAC`; the modern 12-byte header size is confirmed against docs.rs
  when coding.
- **Guest kernel `CONFIG_VIRTIO_NET`** alongside the existing
  `CONFIG_VIRTIO_MMIO`, so the driver binds the MMIO net device without PCI
  probing.
- **Host-side setup outside the VMM** — a `scripts/` helper and `just` recipe
  that create the tap, assign the `/30`, enable forwarding, and install the NAT
  rule, keeping `CAP_NET_ADMIN` in the setup step rather than the VMM
  ([[0010-guest-isolation-jailer]] tracks the eventual jailer).
- **The prior device on the substrate**, [[0004-block-storage-via-virtio-blk]],
  the first virtio-mmio device; net claims the next free slot after it.

## Implementation Phases

Five phases, each of which keeps `cargo build` and the non-gated tests green. The
device and its frame handling land first behind a fake tap, then the real tap and
data path, then host and guest configuration, then the SSH acceptance gate.

### Phase 1: The virtio-net device and CLI flags

Implement the device-side logic as a `VirtioDevice` (from IMPL-0003) with two
virtqueues, the 12-byte header, MAC config space, and a lean feature set, and add
the CLI surface. The tap is abstracted behind a trait so the whole device is
unit-testable with a fake sink and no KVM.

#### Tasks

- [ ] Add `crates/naos-linux/src/net.rs` implementing the `VirtioDevice` trait
  (`device_type` = 1; `queue_max_sizes` for RX queue 0 and TX queue 1) from
  IMPL-0003's `virtio` module.
- [ ] Define a `VIRTIO_NET_HDR_LEN = 12` constant and a `virtio_net_hdr` layout
  (via `virtio-bindings` or a local `#[repr(C)]`); confirm the modern 12-byte
  size against docs.rs.
- [ ] Advertise `VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC` in `device_features`,
  record the acked subset in `ack_features`, and leave checksum, GSO, mergeable
  RX, control-queue, and multiqueue bits off.
- [ ] Serve the 6-byte MAC from `read_config` at config offset 0 and store the
  MAC on the device struct.
- [ ] Define a `Tap` trait (`read_frame` / `write_frame`) with a `Vec`-backed
  fake for tests, so `net.rs` has no `/dev/net/tun` dependency yet.
- [ ] Add `--net`, `--tap <name>` (default `naos-tap0`, implies `--net`), and
  `--mac <addr>` flags to `Args` in `main.rs`; existing invocations are unchanged
  when they are absent.
- [ ] When `--net` is set, construct the net device, register it into the
  `MmioBus`/transport at the next free device slot after the block device, and
  append its `virtio_mmio.device=<size>@<addr>:<irq>` token to the cmdline
  (reusing IMPL-0003's registration path).
- [ ] Unit tests: header length is exactly 12; feature bits match the intended
  set; the MAC reads back from config space; CLI defaults and overrides parse;
  `--net` absent creates no device.

#### Success Criteria

- `cargo build -p naos-linux` and `cargo test -p naos-linux` pass.
- Booting with `--net` shows the guest binding virtio-net (visible in `dmesg` and
  `ip link`) even before frames flow.
- Existing serial-console and block invocations are byte-for-byte unchanged
  without `--net`.

### Phase 2: Tap backend and the frame data path

Open the real tap, register its fd with the event loop, and implement both
directions of the data path with Firecracker-style defer-and-re-arm backpressure
on RX so no frame is lost.

#### Tasks

- [ ] Add `crates/naos-linux/src/tap.rs`: open `/dev/net/tun`, issue `TUNSETIFF`
  with `IFF_TAP | IFF_NO_PI` via a `libc::ioctl` wrapper over `ifreq`, set the fd
  non-blocking, and implement the Phase 1 `Tap` trait over the real fd.
- [ ] Register the tap fd with the `event-manager` epoll loop (IMPL-0001) for
  read readiness; on wake, read one frame and deliver it to the RX queue.
- [ ] RX path: pop an RX descriptor chain, write a 12-byte zero `virtio_net_hdr`
  then the frame via `vm-memory`, `add_used` with the total length, and pulse the
  RX `irqfd` through the `Interrupt` handle.
- [ ] RX backpressure: when the RX available ring is empty, stop reading the tap
  (defer) and re-arm once the guest posts buffers (an RX `QueueNotify`), so the
  frame is deferred rather than dropped.
- [ ] TX path: on the TX `ioeventfd` wake, drain the TX available ring, skip the
  12-byte header, gather the Ethernet frame from guest memory, `write()` it to
  the tap fd, `add_used`, and pulse the TX `irqfd`.
- [ ] Bound both paths per wake (at most a queue's worth of buffers), then yield
  back to epoll so neither fd source starves the other.
- [ ] Tests: the tap ioctl wrapper builds the right `ifreq` (unit); RX/TX ring
  round-trips against the fake tap (unit); a KVM-gated tap loopback (write a frame
  in, assert RX delivery; enqueue on TX, assert it appears on the tap).

#### Success Criteria

- With `--net` and a manually addressed tap, the guest ARPs and pings the host
  and back.
- Under RX pressure with no posted buffers, frames are deferred, not dropped (no
  loss across an `iperf` or `ssh` run).
- No userspace MMIO round-trip on the TX fast path (`ioeventfd`) or the RX
  interrupt path (`irqfd`).

### Phase 3: Host connectivity and guest addressing

Provide the privileged host-side setup outside the VMM, plus the deterministic
MAC and static addressing that make the link usable.

#### Tasks

- [ ] Add `scripts/naos-net.sh` (up/down) that creates `naos-tap0`, assigns the
  host `/30` (for example `10.0.15.1/30`), enables `net.ipv4.ip_forward`, and
  installs an `nftables` masquerade rule on the uplink; teardown reverses it.
- [ ] Add `just net-up` and `just net-down` recipes wrapping the script; document
  the bridge variant alongside.
- [ ] Implement the deterministic MAC: hash the tap name into the low octets and
  fix the first octet to a locally-administered unicast value (set bit 1, clear
  bit 0); `--mac` overrides it.
- [ ] Append an `ip=` token (guest `10.0.15.2`, gateway `10.0.15.1`, `/30` mask,
  `eth0`) to the cmdline when `--net` is set, so the guest takes its static
  address from the kernel.
- [ ] Document that naos only attaches to the tap fd — `CAP_NET_ADMIN` stays in
  the setup script, not the VMM — with the pre-created-tap mode as the interim
  mitigation for [[0010-guest-isolation-jailer]].
- [ ] Tests: the MAC derivation is deterministic and locally-administered for a
  given tap name (unit); the assembled `ip=` token matches the `/30` (unit).

#### Success Criteria

- After `just net-up`, a `--net` guest reaches the host at `10.0.15.1` and the
  LAN or internet via NAT.
- The guest MAC is stable across runs without `--mac` and overridable with it.
- The VMM process holds no `CAP_NET_ADMIN`.

### Phase 4: Guest rootfs with sshd

Build a guest image that comes up on its `/30` address with an SSH server
listening and a user to log in as.

#### Tasks

- [ ] Add `CONFIG_VIRTIO_NET` to the guest kernel config (alongside the existing
  `CONFIG_VIRTIO_MMIO`) and rebuild via the kernel build script.
- [ ] Add an SSH server to the rootfs — `dropbear` for the minimal image, or
  `openssh-server` for a fuller one.
- [ ] Generate SSH host keys and a login user (public-key auth preferred) into
  the rootfs at build time.
- [ ] Add a network-config init step that brings `eth0` up on `10.0.15.2/30` with
  a default route via `10.0.15.1` (or relies on the kernel `ip=` token) before
  `sshd` starts.
- [ ] Add or extend a `scripts/` helper and a `just` target that builds the
  rootfs image so the step is reproducible.
- [ ] Tests: a boot check that the guest reaches `sshd` listening (gated /
  manual, folded into the Phase 5 end-to-end).

#### Success Criteria

- The guest boots with `--net`, `eth0` comes up on the `/30`, and `sshd` is
  listening on port 22.
- The image build is reproducible from a documented recipe.

### Phase 5: Frame-path tests and the SSH acceptance gate

Lock in the fiddly frame handling with host-only unit tests, then stand up the
SSH end-to-end as a documented manual / self-hosted gate.

#### Tasks

- [ ] Unit: the virtio-net header — RX writes an all-zero 12-byte header ahead of
  the frame; TX skips exactly the header and recovers the original Ethernet
  bytes.
- [ ] Unit: backend queue handling with a `vm-memory` fixture and hand-built
  `virtio-queue` rings — RX delivers to the used ring with the right length; TX
  hands the right bytes to the fake tap.
- [ ] Unit: feature negotiation and config space (advertised bits, MAC readback),
  plus CLI parsing for `--net`, `--tap`, and `--mac`.
- [ ] Add `crates/naos-linux/tests/net_e2e.rs`: with `--net`, wait for the guest
  IP to answer, run `ssh user@10.0.15.2 'uname -a'`, assert the output and a zero
  exit; skip cleanly without `/dev/kvm` and a tap, mirroring `boot_e2e.rs`.
- [ ] Add a `just ssh-gate` recipe that runs `net-up`, boots the guest, runs the
  SSH check, and tears down — the documented manual acceptance path.
- [ ] Document the manual gate and its prerequisites (`/dev/kvm`, `CAP_NET_ADMIN`
  for setup, a built rootfs) in the crate or DEVELOPMENT docs.

#### Success Criteria

- `cargo test -p naos-linux` (unit and host-only) is green without KVM.
- On a KVM-capable, privileged host, `ssh user@<guest-ip>` runs a command and
  exits cleanly — the milestone.

## Open Questions

Implementation-level decisions to settle while coding. Option **a** is the
recommendation; **b** onward are alternatives; **other** is a write-in.

### 1. Tap open and attach semantics

The exact TUN/TAP ioctl setup and whether naos creates the tap or only attaches
to a pre-created one.

- **a** (recommended) — `TUNSETIFF` with `IFF_TAP | IFF_NO_PI`, multiqueue off,
  and attach-only to a tap the setup script created; naos never holds
  `CAP_NET_ADMIN`. Matches decision 6 and keeps the privilege surface in one
  scripted place.
- **b** — let naos create the tap on first attach, which requires
  `CAP_NET_ADMIN` in the VMM for the VM's whole lifetime.
- **other** — *write-in*

**Decision:** a — `TUNSETIFF` with `IFF_TAP | IFF_NO_PI`, multiqueue off, attach-only to a tap the setup script created; naos never holds `CAP_NET_ADMIN`.

### 2. Defer and re-arm against the event loop

How RX defer-and-re-arm is implemented against `event-manager` — level versus
edge readiness, and when to re-check or re-register the tap fd.

- **a** (recommended) — keep the tap fd registered level-triggered; on an empty
  RX ring, stop reading and set a deferred flag, then resume draining the tap when
  the next RX `QueueNotify` shows posted buffers — no fd churn, simplest against
  level semantics.
- **b** — deregister the tap fd while deferred and re-register it when buffers are
  posted (edge-style), avoiding spurious wakeups at the cost of fd churn per
  stall.
- **other** — *write-in*

**Decision:** a — keep the tap fd registered level-triggered; on an empty RX ring, stop reading and set a deferred flag, then resume draining on the next RX `QueueNotify`.

### 3. The MAC derivation function

How the deterministic locally-administered MAC is derived from the tap name.

- **a** (recommended) — hash the tap name (for example FNV or SipHash) into the
  low five octets and fix the first octet to a locally-administered unicast value
  (set bit 1, clear bit 0, for example `0x02`). Deterministic, collision-unlikely
  for a handful of taps, and standards-clean.
- **b** — a fixed vendor-style prefix plus a tap index or counter instead of a
  hash.
- **other** — *write-in*

**Decision:** a — hash the tap name into the low five octets and fix the first octet to a locally-administered unicast value (set bit 1, clear bit 0).

### 4. Scripting the manual SSH gate

How the SSH acceptance test is scripted and documented so it is runnable but not
required in CI.

- **a** (recommended) — a `just ssh-gate` recipe (net-up, boot, SSH check,
  teardown) plus a gated `tests/net_e2e.rs` that skips without `/dev/kvm` and a
  tap, documented in DEVELOPMENT. Consistent with the existing KVM-gated
  `boot_e2e` pattern.
- **b** — a self-hosted CI runner that grants `CAP_NET_ADMIN` and runs the gate
  on every push.
- **other** — *write-in*

**Decision:** a — a `just ssh-gate` recipe plus a gated `tests/net_e2e.rs` that skips without `/dev/kvm` and a tap, documented in DEVELOPMENT.

## File Changes

| File | Action | Description |
|------|--------|-------------|
| `crates/naos-linux/src/net.rs` | Create | virtio-net `VirtioDevice` impl: RX/TX queues, 12-byte header, feature set, MAC config space, queue handling over a `Tap` trait. |
| `crates/naos-linux/src/tap.rs` | Create | `/dev/net/tun` open, `TUNSETIFF(IFF_TAP\|IFF_NO_PI)` ioctl wrapper, non-blocking fd, `Tap` trait plus a fake sink for tests. |
| `crates/naos-linux/src/main.rs` | Modify | Add `--net`, `--tap`, and `--mac` flags; keep existing invocations unchanged when absent. |
| `crates/naos-linux/src/vmm.rs` | Modify | When `--net` is set, construct the net device, register it into the `MmioBus`, wire the TX `ioeventfd` and RX/TX `irqfd`, subscribe the tap fd to the event loop, and append the `virtio_mmio.device=` and `ip=` cmdline tokens. |
| `crates/naos-linux/Cargo.toml` | Modify | Depend on `virtio-bindings` for `virtio_net_hdr` and `VIRTIO_NET_F_MAC` (reused from IMPL-0003; add if not already present). |
| `scripts/naos-net.sh` | Create | Create/tear down the tap, assign the `/30`, enable forwarding, install the NAT rule; the bridge variant documented alongside. |
| `Justfile` | Modify | Add `net-up`, `net-down`, and `ssh-gate` recipes; document a `--net` run. |
| `crates/naos-linux/tests/net_e2e.rs` | Create | Gated SSH acceptance test; skips cleanly without `/dev/kvm` and a tap. |
| Guest rootfs / kernel build (`scripts/`) | Modify | Add `CONFIG_VIRTIO_NET`, an SSH server, host keys, a login user, and the network-config init step. |

## Testing Plan

- [ ] Unit: virtio-net header — length is exactly 12, RX writes an all-zero
  header, TX skips exactly the header and recovers the frame.
- [ ] Unit: RX/TX ring round-trips against the fake tap (`vm-memory` plus
  hand-built `virtio-queue` rings).
- [ ] Unit: feature bits match the intended set and the MAC reads back from
  config space.
- [ ] Unit: deterministic MAC derivation is stable and locally-administered; the
  `ip=` token matches the `/30`.
- [ ] Unit: CLI parsing for `--net`, `--tap`, and `--mac` defaults and overrides.
- [ ] KVM-gated: tap loopback — a frame written in reaches the RX queue, and a TX
  frame appears on the tap.
- [ ] KVM-gated: the guest boots with `--net`, `eth0` appears (`ip link`), and
  ARP/ping to the host works.
- [ ] Manual / self-hosted gate: `ssh user@<guest-ip> 'uname -a'` returns the
  output and exit code 0.

## References

- [[0005-guest-networking-and-ssh]] — source design
- [[0003-virtio-mmio-device-model]] — the substrate: transport, `MmioBus`,
  `VirtioDevice` trait, virtqueues, `irqfd`, and `ioeventfd`
- [[0001-event-loop-and-concurrency-model]] — the event loop, the I/O thread, and
  the tap fd registration path
- [[0004-block-storage-via-virtio-blk]] — the first device on the substrate; the
  plumbing this reuses
- [[0006-guest-networking-via-virtio-net-and-tap]] — the ADR the design implements
- [[0010-guest-isolation-jailer]] — the deferred jailer for the `CAP_NET_ADMIN`
  gap
- virtio 1.2 specification, section 5.1 (Network Device) and 5.1.6 (`virtio_net_hdr`,
  the 12-byte header): <https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html>
- Linux TUN/TAP — `/dev/net/tun`, `TUNSETIFF`, `IFF_TAP`, `IFF_NO_PI`:
  <https://docs.kernel.org/networking/tuntap.html>
- rust-vmm crates: [`virtio-device`](https://docs.rs/virtio-device),
  [`virtio-queue`](https://docs.rs/virtio-queue),
  [`virtio-bindings`](https://docs.rs/virtio-bindings),
  [`vm-memory`](https://docs.rs/vm-memory),
  [`vmm-sys-util`](https://docs.rs/vmm-sys-util),
  [`kvm-ioctls`](https://docs.rs/kvm-ioctls)
- Code: `crates/naos-linux/src/main.rs`, `vmm.rs`, `vcpu.rs`, `serial.rs`,
  `memory.rs`, `boot.rs`; `crates/naos-linux/tests/boot_e2e.rs`; `Justfile`;
  `scripts/`
