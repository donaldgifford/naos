# ARCHITECTURE

This document describes what naos is, why it exists, and how it is structured as
a project. It is the load-bearing context for everything else in the repository.
Implementation details for individual components live in their own design docs
under `docs/decisions/`; this document does not restate them.

## What naos is

naos is a custom Rust virtual machine monitor (VMM) platform, built from first
principles, optimized for learning and for the specific operational preferences
of its author. It is not a competitor to Firecracker, Cloud Hypervisor, libkrun,
or QEMU. It is a place to understand how VMMs work at the level of registers and
page tables, and then to grow that understanding into a platform that runs
workloads the way we want them run.

The word "platform" is doing real work here. naos is not planned as a single
binary. It is planned as a set of crates — hypervisor backends, device
emulation, a management CLI, eventually an API and a jailer — that together form
an opinionated system for running microVMs. The hypervisor piece is the
foundation and the current focus, but it exists to serve the larger platform,
not as the end goal.

## Why naos exists

Three reasons, in decreasing order of importance.

**Learning by building.** The rust-vmm ecosystem and the existing VMM projects
(Firecracker, Cloud Hypervisor, crosvm, libkrun) are excellent and widely used,
but using them as a black box leaves the internals opaque. Writing a VMM from
first principles — constructing the GDT by hand, walking the page table entries,
configuring sregs to drop a vCPU into long mode, wiring a 16550 UART to a PIO
port — forces genuine understanding of how hardware virtualization actually
works. Every file in naos is an excuse to read the Intel SDM or the Apple
Silicon reference manual and turn a paragraph of specification into running
code.

**Ownership of the stack.** Existing VMM projects make design decisions driven
by their problem domains. Firecracker is optimized for serverless cold-start.
libkrun is optimized for container sandboxing with an opinionated baked-in
kernel. Cloud Hypervisor targets cloud workloads. Each of these projects
inherits a set of opinions that leak into every part of the codebase. Building
naos means forming our own opinions — and importantly, being able to discover
that other people's opinions were right or wrong by comparison. If naos ends up
looking a lot like Firecracker in places, we will know _why_ Firecracker made
those choices, not just _that_ it did. If naos diverges, we will have earned the
divergence.

**A platform we actually want to use.** The author runs workloads on a homelab
of Dell PowerEdge servers and a NixOS workstation, and develops on an Apple
Silicon MacBook Pro. The gap between "VMM that runs on Linux servers" and "VMM
that runs on the Mac you actually type on" is real and annoying. Existing tools
address pieces of this (libkrun runs on both, Firecracker runs on neither's
native hypervisor on the Mac side, Lima fakes it with a Linux VM in the middle),
but none of them are ours and none of them make the exact trade-offs we would
make. naos is the platform we would build if we were building exactly what we
want.

## First-principles philosophy

naos is built to a specific aesthetic, which shows up in every design decision
and should be preserved when extending the project.

**Minimum viable everything.** Each component is scoped to the smallest thing
that can possibly work before anything is added. The first hypervisor boots a
kernel to dmesg and exits — no rootfs, no network, no shell. The first device is
one UART, not a device bus. The first vCPU count is one. Every feature beyond
this baseline earns its place by being demanded by a concrete next goal, not by
speculation about future needs.

**Comments over abstraction.** Files are small and heavily commented. When a
constant comes from a specification, the comment names the specification
section. When a sequence of ioctls has a specific order that matters, the
comment explains why. The test for a comment is whether future-us, auditing the
code a year from now, can understand _why_ the code does what it does without
re-reading the manual. Abstractions are added only when two concrete use cases
force them — never prophylactically.

**Concrete before abstract.** naos will eventually have a hypervisor abstraction
layer that lets the same VMM code run on Linux KVM and macOS
Hypervisor.framework. That layer will be designed _after_ both concrete backends
exist, not before. Premature abstraction is the single most common failure mode
of ambitious systems projects, and naos explicitly rejects it. We will write two
backends, live with the duplication, and extract the abstraction from the
reality of what they share — not from a guess about what they might share.

**No dependencies we wouldn't write ourselves.** Every dependency in
`Cargo.toml` is there because writing it ourselves would be clearly worse use of
time. `kvm-ioctls` is in because rewriting the KVM ioctl layer is pure busywork.
`linux-loader` is in because ELF parsing is a solved problem. But
`event-manager` is out for the MVP because one blocking thread is simpler than
introducing a framework, and `virtio-*` crates are out until a device forces
them. The bar is "does adding this make the code simpler or smaller than not
adding it," and the default answer is no.

**Opinions, not options.** naos does not expose configuration for things that
should not be configured. The first backend is x86_64 only. The first kernel
format is vmlinux ELF only. The first memory layout is hardcoded. These are not
limitations to be removed — they are decisions. Every configuration knob is a
commitment to support N × M combinations forever; we add knobs only when we
genuinely need them.

## Project ladder

naos is built in stages, each of which produces a usable artifact and teaches
something specific. The stages are independent — each can be designed, built,
and reasoned about without the others existing — but they are also ordered,
because later stages depend on what is learned in earlier ones.

**Stage 1: naos-linux.** A minimum viable hypervisor built on the rust-vmm crate
ecosystem, targeting Linux hosts with KVM and x86_64 guests. Boots a Linux
vmlinux ELF to dmesg and exits cleanly. Teaches x86_64 VMM internals: long mode
entry, GDT construction, page table setup, sregs, vCPU exit dispatch, PIO device
emulation. Design lives in `docs/decisions/` under a separate DESIGN doc. This
is the current focus of the project.

**Stage 2: naos-macos.** A minimum viable hypervisor targeting macOS hosts with
Hypervisor.framework and aarch64 guests (on Apple Silicon). Parallel to
naos-linux in philosophy and scope — boots an aarch64 Linux kernel to dmesg and
exits cleanly — but built against a different hypervisor API and a different
guest architecture. Teaches aarch64 VMM internals and Hypervisor.framework.
Design lives in its own DESIGN doc, written before work starts so the shape of
the project is clear even if the details are not. Stage 2 begins after stage 1
is complete.

**Stage 3: naos-vmm.** A hypervisor abstraction layer extracted from naos-linux
and naos-macos once both exist and work. This crate does not have a design doc
yet and will not until stage 2 is complete, because designing it before both
concrete implementations exist would be pure speculation. When the time comes,
the trait will be extracted from what the two backends actually share — not from
what we imagine they might share. naos-linux and naos-macos will then be
refactored to implement the trait, and future backends (Hyper-V? something
else?) can be added behind the same interface.

Beyond stage 3, the platform grows into additional crates — naos-cli, naos-api,
naos-jailer, device emulation crates, virtio implementations — as specific needs
arise. None of these are committed to yet, and none have design docs. They are
mentioned here only to establish that naos is planned as a multi-crate platform,
not as a single binary.

## Workspace layout

The Cargo workspace is structured to reflect the project ladder. Crates are
added as they are built, not speculatively.

```
naos/
├── Cargo.toml              # workspace manifest
├── ARCHITECTURE.md         # this document
├── DEVELOPMENT.md          # dev environment setup
├── docs/decisions/         # design docs, ADRs, RFCs (managed via docz CLI)
└── crates/
    └── naos-linux/          # stage 1: KVM + x86_64 hypervisor (current focus)
    # crates/naos-macos/     # stage 2: added when stage 1 is complete
    # crates/naos-vmm/       # stage 3: extracted from stages 1 and 2
```

The `naos-macos` and `naos-vmm` crates are deliberately absent from the
workspace until the work on them actually begins. Placeholder crates tend to
accumulate speculative code and create the false impression that something
exists when it does not.

## Naming conventions

The crate names are chosen to describe what varies and to match how we talk
about the project in conversation.

- **naos-linux** is the Linux-host, x86_64-guest hypervisor. The name collapses
  host OS and guest architecture into a single identifier, which is honest for
  this project because we do not plan to ship an aarch64 Linux backend or an
  x86_64 macOS backend. If that ever changes, the names get revisited.
- **naos-macos** is the macOS-host, aarch64-guest hypervisor. Same reasoning.
- **naos-vmm** is the future abstraction crate. The bare name reflects its
  status as the load-bearing interface — it is _the_ VMM trait layer, and the
  backends are implementations of it.

We explicitly reject `naos-kvm` / `naos-hvf` (names the hypervisor API instead
of the host, which foregrounds an implementation detail) and `naos-linux-x86_64`
/ `naos-macos-arm64` (verbose, never actually typed).

## Relationship to other VMM projects

naos exists in a field of high-quality prior art. Being explicit about where
naos differs helps clarify what it is.

**rust-vmm.** naos is built on rust-vmm crates (`kvm-ioctls`, `vm-memory`,
`linux-loader`, `vm-superio`) for the Linux backend. rust-vmm is the foundation,
not the competition. naos is one of many possible VMMs that could be built on
top of it — Firecracker, Cloud Hypervisor, and crosvm are others. The
distinctive thing about naos is not the primitives it uses but the opinions and
the scope it applies them at.

**Firecracker.** Firecracker is the proximate inspiration for naos — a minimal,
opinionated, KVM-based microVM hypervisor written in Rust. naos will likely end
up looking similar to Firecracker in many places, and that is fine. Where naos
differs is in being smaller, more heavily commented, and designed by one person
for one person's workflow rather than for production serverless at scale.

**Cloud Hypervisor.** Cloud Hypervisor has already built part of what naos stage
3 is aiming at — a VMM abstracted behind a hypervisor trait, with multiple
backends (KVM and Microsoft's MSHV). naos could in principle contribute a
Hypervisor.framework backend to Cloud Hypervisor instead of building its own,
and that would be a legitimate alternative path. naos takes the own-project path
because learning and ownership are primary goals, not because Cloud Hypervisor
is wrong.

**libkrun.** libkrun is a C library that exposes a VMM as a linkable library
with backends for both KVM and Hypervisor.framework. It is the closest existing
thing to what naos stage 3 is aiming at. naos takes a different path because
libkrun is opinionated toward container sandboxing (it ships a specific kernel,
libkrunfw, and a specific use case), and because naos wants to be in pure Rust
end-to-end, and because the learning-by-building goal is served better by
writing the code than by calling into someone else's C library.

**QEMU.** QEMU is in a different category — a mature, general-purpose,
full-system emulator and VMM. naos has no ambition to compete with QEMU on
feature surface. QEMU is the comparison point for "what a full VMM looks like"
and nothing more.

## Non-goals for the platform

These are naos-wide non-goals, distinct from the scope-limiting non-goals of any
individual stage. They apply to the project as a whole, and are not things that
become goals in later stages.

- **Production deployment at scale.** naos is not aiming to be the hypervisor
  behind a serverless platform or a public cloud. If it ends up good enough for
  that, great, but that is not the design pressure.
- **Performance parity with Firecracker or Cloud Hypervisor.** naos optimizes
  for comprehensibility and correctness, not nanoseconds. Performance work is
  not a non-goal forever — eventually it will matter — but it is not a design
  pressure for the foundational stages.
- **Competing with existing VMMs for users.** naos is built for its author and
  for anyone who finds the first-principles approach educational. It is not
  chasing a user base.
- **Supporting every host OS and guest architecture.** naos supports exactly the
  combinations its author runs: Linux/x86_64 and macOS/aarch64. Windows hosts,
  Intel Macs, aarch64 servers, and other combinations are out of scope unless
  and until someone who uses them is building naos.
- **Replacing containers.** naos is a VMM project, not a container runtime. VMs
  and containers have different properties and different use cases; naos is
  firmly on the VM side of that line.

## Open architectural questions

These are questions that do not need to be answered yet but that future work
will eventually force. They are listed here so they are not forgotten.

- **Process model.** Will naos be a single process per VM (Firecracker-style) or
  a long-running daemon that manages multiple VMs (libvirt-style)? The MVP
  sidesteps this by running exactly one VM and exiting. Stage 3 or later will
  force the decision.
- **API surface.** Firecracker exposes an HTTP API over a Unix socket. Cloud
  Hypervisor has its own. naos has none yet. The right answer depends on how
  naos is used, which we do not know yet.
- **Jailer and sandboxing.** The MVP runs naos as a normal user process. A real
  platform needs a jailer (seccomp, namespaces, chroot) to run untrusted guest
  kernels safely. This is a real concern but not a foundational one — it gets
  designed when it becomes load-bearing.
- **Snapshot and restore.** Firecracker's killer feature. naos has no snapshot
  support and no plan for one yet. Adding it later requires careful attention to
  device state and memory layout, which is another reason to build the
  foundations carefully first.
