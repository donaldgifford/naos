# DEVELOPMENT

How to build, run, and hack on naos.

This guide currently covers `naos-linux`, the Linux/KVM hypervisor backend — the
first stage of the naos project ladder (see `ARCHITECTURE.md` for context).
`naos-macos` will get its own setup instructions when work on that crate begins;
until then, macOS is not a supported development environment for naos, for
reasons detailed at the end of this document.

The reference development machine is Donald's NixOS workstation (`workstation`),
but any modern x86_64 Linux box with `/dev/kvm` works.

## Prerequisites

naos-linux needs:

- **An x86_64 Linux host with KVM enabled.** Verify with `ls -l /dev/kvm` (the
  device must exist) and `lsmod | grep kvm` (the `kvm` and `kvm_intel` or
  `kvm_amd` modules must be loaded). Both Intel VT-x and AMD SVM hosts work —
  naos-linux uses the vendor-neutral KVM API.
- **Membership in the `kvm` group** so your user can open `/dev/kvm` without
  root. Check with `id | grep kvm`. If missing: `sudo usermod -aG kvm $USER` and
  log out/in.
- **Rust stable toolchain.** Whatever rustup gives you. naos pins the toolchain
  via `rust-toolchain.toml` in the repo root, so the first `cargo` invocation
  will fetch the right version.
- **Build essentials for the test kernel:** `gcc`, `make`, `flex`, `bison`,
  `bc`, `libelf-dev` (or your distro's equivalent), `libssl-dev`. On NixOS these
  come in via a one-shot `nix-shell -p` invocation documented below — we are
  deliberately not putting naos itself behind a flake.

Quick sanity check before going further:

```bash
ls -l /dev/kvm                    # crw-rw---- root kvm
id | tr ',' '\n' | grep kvm       # should show kvm group
rustc --version                   # any recent stable
```

If all three pass, you can build naos.

## First build

```bash
git clone <naos-repo-url> ~/code/naos
cd ~/code/naos
just check
```

`just check` detects your host OS and runs `cargo check` against the appropriate
crate (`naos-linux` on Linux hosts, `naos-macos` on macOS hosts). If this
succeeds, your toolchain and dependencies are wired up correctly. See the
"Editor setup" and "Running commands: Justfile" sections below for more on how
the Justfile works.

To build for real:

```bash
just build
```

The workspace currently contains one crate: `crates/naos-linux`. Additional
crates (`naos-macos`, `naos-vmm`, and others) will appear under `crates/` as the
project ladder progresses — see `ARCHITECTURE.md`.

To run the test suite:

```bash
just test
```

The test suite is small by design — see the naos-linux design doc under
`docs/decisions/` for what's covered and what isn't.

## Building the test kernel

naos-linux boots a Linux vmlinux ELF. We build our own from source against
`tinyconfig` rather than fetching a prebuilt, for the same reason naos itself is
built from first principles: we want to know exactly what's in the kernel we're
booting and why.

The test kernel lives outside the naos repo (it's large and not source code we
own). Convention: build it under `~/src/linux` and symlink the resulting vmlinux
into `~/code/naos/testdata/vmlinux`. When `naos-macos` is ready, it will need
its own aarch64 test kernel under `testdata/aarch64/` — instructions will be
added here at that time.

### One-time setup

On NixOS, drop into a shell with the kernel build dependencies:

```bash
nix-shell -p gcc gnumake flex bison bc elfutils openssl pkg-config ncurses
```

On Debian/Ubuntu:

```bash
sudo apt install build-essential flex bison bc libelf-dev libssl-dev libncurses-dev
```

### Fetch and configure

```bash
mkdir -p ~/src && cd ~/src
git clone --depth 1 --branch v6.12 https://github.com/torvalds/linux.git
cd linux

# Start from the smallest possible config.
make tinyconfig

# Enable just enough to boot under naos and print to serial.
# Each option below has a specific reason — do not add more without justification.
./scripts/config \
  --enable 64BIT \
  --enable PRINTK \
  --enable EARLY_PRINTK \
  --enable TTY \
  --enable SERIAL_8250 \
  --enable SERIAL_8250_CONSOLE \
  --enable BINFMT_ELF

# Resolve any new dependencies tinyconfig didn't pull in.
make olddefconfig
```

What each option buys us:

- **64BIT** — naos-linux boots the vCPU directly into long mode; we need a
  64-bit kernel.
- **PRINTK / EARLY_PRINTK** — without these, the kernel boots silently and we
  see nothing. EARLY_PRINTK is what gets us output before the full console
  subsystem initializes.
- **TTY / SERIAL_8250 / SERIAL_8250_CONSOLE** — drives the 16550 UART that
  naos-linux emulates. Without these the kernel boots but never writes to our
  serial port.
- **BINFMT_ELF** — strictly speaking only needed once we have userspace, but
  tinyconfig disables it and re-enabling now avoids surprise later.

### Build

```bash
make -j$(nproc) vmlinux
```

This produces `vmlinux` (the ELF naos-linux loads) in the kernel source root. On
a workstation-class machine this is a 3–5 minute build the first time, much
faster on subsequent rebuilds.

### Wire it into naos

```bash
mkdir -p ~/code/naos/testdata
ln -sf ~/src/linux/vmlinux ~/code/naos/testdata/vmlinux
```

`testdata/` is gitignored — the vmlinux is a build artifact, not source.

### Shortcut: the kernel-build script

The "one-time setup", "Fetch and configure", "Build", and "Wire it into naos"
steps above are bundled into `scripts/build-test-kernel-x86_64.sh`, invoked via
the Justfile:

```bash
just kernel-linux
```

The script is idempotent — running it again after the kernel source has pulled
new changes rebuilds only what changed and refreshes the symlink. It respects
the `NAOS_KERNEL_SRC` environment variable if your kernel source lives somewhere
other than `~/src/linux`. You still need the kernel source cloned and the
build-essential packages installed (the one-time setup steps above); the script
covers configure, build, and symlink.

## Running naos-linux

With a built binary and a test kernel in place:

```bash
cd ~/code/naos
just run --kernel testdata/vmlinux --mem 256
```

Or equivalently, without `just`:

```bash
cargo run -p naos-linux -- --kernel testdata/vmlinux --mem 256
```

What success looks like: a stream of Linux kernel boot messages on stdout,
ending in a kernel panic about a missing init process, followed by naos-linux
exiting cleanly with status 0. That panic is the success signal — it means the
kernel booted, ran, and got far enough to look for userspace. See the naos-linux
design doc for the rationale.

A minimal expected trace looks roughly like:

```
[    0.000000] Linux version 6.12.0 ...
[    0.000000] Command line: console=ttyS0 reboot=k panic=1 pci=off
[    0.000000] BIOS-provided physical RAM map:
...
[    0.123456] Run /init as init process
[    0.123789] Kernel panic - not syncing: No working init found.
```

If you see those lines, the MVP works.

## When it doesn't work

The failure modes cluster into a few categories. Diagnose in this order:

**1. naos-linux exits before any kernel output.** Setup failure — KVM
unavailable, kernel file missing, ELF parse failure, memory mmap failure. The
error message and chain on stderr should tell you which step. If it's a
permissions error on `/dev/kvm`, check the `kvm` group membership.

**2. naos-linux runs but no kernel output appears.** Either the vCPU isn't
reaching the kernel entry point, or the kernel is running but can't use the
serial port. Check:

- The kernel was built with `SERIAL_8250_CONSOLE` enabled.
- The default cmdline includes `console=ttyS0` (it does, unless you overrode
  `--cmdline`).
- The vCPU isn't immediately exiting with an unexpected exit reason — naos-linux
  logs these to stderr.

**3. naos-linux exits with an unexpected vCPU exit.** This means the guest is
hitting an instruction or I/O port naos-linux doesn't handle. Log the exit
variant and the guest RIP if available. Usually this points at a bug in
`boot.rs` (wrong sregs, page tables not mapping the kernel) or a missing port in
the serial range check.

**4. The kernel boots, prints messages, but hangs instead of panicking.**
Usually means `panic=1` isn't on the cmdline, or the kernel found something
(like an initrd) it didn't expect to find. Confirm the cmdline naos-linux is
passing.

Useful host-side tools:

- `dmesg | grep -i kvm` on the host — KVM module errors (e.g. nested virt
  issues) show up here.
- `strace -e ioctl cargo run -p naos-linux -- ...` — surfaces every KVM ioctl
  naos-linux issues. Verbose, but the gold standard for "what is the VMM
  actually asking the kernel to do."
- `RUST_BACKTRACE=1` — gets you a stack trace when naos-linux errors out.

## Editor setup

naos is a Cargo workspace with multiple crates, and the two backend crates
(`naos-linux` and `naos-macos`) deliberately do not compile on each other's
host. Without configuration, rust-analyzer tries to index the whole workspace
and fails loudly on the host-wrong crate — red squiggles across a crate you are
not even working on, and wasted CPU indexing code that cannot build.

The fix is to tell rust-analyzer explicitly which crate to index, per-machine,
via its `linkedProjects` setting. The repo ships a `.nvim.lua` at the root that
does this automatically for Neovim users.

### Why `.nvim.lua` and not a user-level config

A few alternatives were considered and rejected:

- **User-level `rust-analyzer.toml` at `~/.config/rust-analyzer/`.** Applies
  globally to every project on the machine, cannot be scoped to the naos repo
  specifically. Wrong tool.
- **Repo-level `rust-analyzer.toml` with both crates in `linkedProjects`.**
  rust-analyzer would still try to index both and fail on the host-wrong one.
  Does not solve the problem.
- **`neoconf.nvim` with explicit allowlisting.** More ceremony, requires a
  plugin. Overkill for a personal project on trusted machines.
- **Committed `.nvim.lua` with OS-detection logic.** This is what the repo uses.
  The config lives with the project, is version-controlled, and adapts to
  whichever machine you are on without per-machine tweaking.

The `.nvim.lua` uses `vim.loop.os_uname().sysname` to detect whether it is
running on Linux or macOS and sets `linkedProjects` to the appropriate crate. On
unknown platforms it does nothing, letting rust-analyzer fall back to default
behavior rather than hard-failing.

### Neovim + rustaceanvim (the supported path)

The reference editor setup is Neovim with LazyVim's `lang.rust` extra, which
installs `rustaceanvim` as the Rust LSP client. If you already run LazyVim with
`lang.rust` enabled in your `lazyvim.json`, `rustaceanvim` is what powers your
Rust editing — no additional setup needed beyond the steps below.

To verify `rustaceanvim` is active: open any Rust file in Neovim, then run
`:LspInfo`. You should see `rustaceanvim` (not bare `rust_analyzer`) attached.
If you see something else, the `.nvim.lua` may still work but the exact Lua
syntax in the committed `.nvim.lua` is targeted at `rustaceanvim`'s config shape
— you may need to adapt it.

For the committed `.nvim.lua` to take effect, your Neovim config must have
`exrc` enabled:

```lua
-- Somewhere in your LazyVim config, e.g. lua/config/options.lua
vim.o.exrc = true
```

`exrc` tells Neovim to load project-local `.nvim.lua` (and `.exrc`, `.vimrc`)
from the current directory. It is off by default because loading arbitrary Lua
from any directory you `cd` into is a real security consideration. Enable it
only if you trust the contents of every directory you open Neovim inside. For
your own `~/code/` tree this is reasonable; for cloning random repos, it is not.

After enabling `exrc` and opening the naos repo in Neovim, verify rust-analyzer
is indexing the right crate:

```
:LspInfo
```

You should see rust-analyzer attached and the workspace scoped to a single
crate. If it is indexing the whole workspace, `.nvim.lua` did not load — check
that `exrc` is set and that `.nvim.lua` exists at the repo root.

### A note on the `.nvim.lua` internals

The committed file uses `vim.g.rustaceanvim` with `default_settings` (not
`settings`) to configure rust-analyzer. This distinction matters: `rustaceanvim`
reads its baseline rust-analyzer config from `default_settings` and merges user
overrides on top. Using `settings` instead would work in plain `nvim-lspconfig`
but not in `rustaceanvim`. The file also uses
`vim.tbl_deep_extend("force", ...)` so any future additions compose cleanly with
the existing config rather than clobbering it.

### Other editors

If you use VS Code, Zed, Helix, or another editor with rust-analyzer support,
set `rust-analyzer.linkedProjects` yourself in the editor's per-workspace
config, pointing at the crate appropriate for your host. For example, in VS
Code's `.vscode/settings.json` (not committed):

```json
{
  "rust-analyzer.linkedProjects": ["crates/naos-linux/Cargo.toml"]
}
```

Swap `naos-linux` for `naos-macos` on macOS hosts. The repo's `.gitignore`
excludes `.vscode/` and `.idea/`, so any per-editor configuration you add stays
local to your machine.

## Running commands: Justfile

The repo includes a `Justfile` at the root with task-runner recipes. Install
`just` via your package manager (`nix-env -iA nixpkgs.just` on NixOS,
`brew install just` on macOS, etc.) and then run `just` with no args to see all
recipes.

The commands you will actually type most of the time are the host-aware
shortcuts, which dispatch to the right crate for the current OS:

```bash
just check              # typecheck the appropriate crate
just build              # build it
just test               # run its tests
just run --kernel testdata/vmlinux --mem 256   # run it with args
just lint               # clippy, warnings as errors
just fmt                # format everything, any host
```

Explicit per-crate recipes are also available — `just check-linux`,
`just build-macos`, `just run-linux --kernel ...` — for cases where you need to
target a specific crate regardless of host.

Running the host-wrong recipe on the host-wrong machine (e.g. `just build-macos`
on Linux) will fail at the dependency-resolution step with a clear error from
Cargo. This is expected — it is how the workspace signals "this crate is not for
this host."

## Why this guide is Linux-only (for now)

naos-linux targets KVM, which is a Linux kernel subsystem (`/dev/kvm`), not a
userspace API. The `kvm-ioctls` and `kvm-bindings` crates are thin wrappers over
Linux ioctls and are gated `#[cfg(target_os = "linux")]` at the crate root —
they refuse to compile on macOS. There is no shim, wrapper, or translation layer
that makes them work on Darwin.

Apple Silicon adds a second wall: even inside a Linux VM on a Mac (Lima, UTM,
etc.), the guest is aarch64, not x86_64. naos-linux is x86_64-only and you
cannot nest a different CPU architecture — there is no x86_64 silicon underneath
an M-series chip to virtualize.

**This is why `naos-macos` exists as a separate crate** (see `ARCHITECTURE.md`).
Rather than trying to make naos-linux work on macOS — which is impossible —
stage 2 of the project builds a second VMM from scratch, targeting
Hypervisor.framework directly and running aarch64 Linux guests natively on Apple
Silicon. When that crate is ready, this document will gain a macOS section with
its own setup instructions.

Until then, if you are on a Mac and want to hack on naos-linux, SSH to
`workstation` (or any x86_64 Linux box with KVM) and do everything there.
Editing on the Mac and running on the Linux box is a fine setup — it is what
most VMM developers with Apple Silicon machines do.

## Repo conventions

- **Workspace root** is `~/code/naos` by convention. Nothing depends on this
  path.
- **Workspace manifest** at the root (`Cargo.toml`) declares member crates and
  workspace-wide lints.
- **Toolchain** is pinned to current stable via `rust-toolchain.toml` at the
  root. rustup fetches the correct version on first `cargo` invocation.
- **Crates** live under `crates/`. Stage 1 ships `crates/naos-linux`;
  `crates/naos-macos` and `crates/naos-vmm` appear as their stages begin. See
  `ARCHITECTURE.md` for the ladder.
- **Test artifacts** (like the test vmlinux) live under `testdata/` and are
  gitignored. aarch64 test kernels for `naos-macos` will live under
  `testdata/aarch64/` when that crate is built.
- **Build scripts** live under `scripts/`. The Justfile wraps them with
  host-aware recipes.
- **Documentation** lives in two places: `ARCHITECTURE.md` and `DEVELOPMENT.md`
  at the repo root for project-level and operational context, and design docs
  under `docs/decisions/` managed via the `docz` CLI for per-component design.
- **Project-local editor config** is limited to `.nvim.lua` (committed). Other
  editor-specific configs (`.vscode/`, `.idea/`, etc.) are gitignored.
- **Nix.** naos itself is not behind a flake. The kernel build dependencies are
  pulled in via a one-shot `nix-shell -p` when needed. This is a deliberate
  choice — keeping naos toolchain-agnostic means it builds the same way on any
  Linux distro.
