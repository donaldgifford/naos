# Justfile — task runner for naos.
#
# See DEVELOPMENT.md for prerequisites and context. Run `just` with no
# arguments to see the list of recipes.

# List all recipes (default when `just` is invoked with no args).
default:
    @just --list

# =============================================================================
# Host-aware shortcuts
#
# These detect the current OS and dispatch to the right crate-specific
# recipe. This is what you should type 95% of the time — it removes the
# need to remember which crate is valid on which host.
# =============================================================================

# Fast typecheck the crate that makes sense on this host.
check:
    @just check-{{os()}}

# Build the crate that makes sense on this host.
build:
    @just build-{{os()}}

# Run the crate's test suite that makes sense on this host.
test:
    @just test-{{os()}}

# Lint the crate that makes sense on this host.
lint:
    @just lint-{{os()}}

# Run the crate that makes sense on this host. Forwards args to the binary.
#
# Example:
#   just run --kernel testdata/vmlinux --mem 256
run *args:
    @just run-{{os()}} {{args}}

# =============================================================================
# naos-linux
#
# These always target naos-linux regardless of host. On macOS they will
# fail to compile at the kvm-ioctls dependency.
# =============================================================================

# Fast typecheck naos-linux.
check-linux:
    cargo check -p naos-linux

# Build naos-linux in debug mode.
build-linux:
    cargo build -p naos-linux

# Build naos-linux in release mode.
release-linux:
    cargo build -p naos-linux --release

# Run the naos-linux test suite.
test-linux:
    cargo test -p naos-linux

# Lint naos-linux with clippy, warnings as errors.
lint-linux:
    cargo clippy -p naos-linux --all-targets -- -D warnings

# Run naos-linux. Forwards args to the binary.
run-linux *args:
    cargo run -p naos-linux -- {{args}}

# =============================================================================
# naos-macos
#
# These always target naos-macos regardless of host. On Linux they will
# fail to compile at the Hypervisor.framework dependency.
# =============================================================================

# Fast typecheck naos-macos.
check-macos:
    cargo check -p naos-macos

# Build naos-macos in debug mode.
build-macos:
    cargo build -p naos-macos

# Build naos-macos in release mode.
release-macos:
    cargo build -p naos-macos --release

# Run the naos-macos test suite.
test-macos:
    cargo test -p naos-macos

# Lint naos-macos with clippy, warnings as errors.
lint-macos:
    cargo clippy -p naos-macos --all-targets -- -D warnings

# Run naos-macos. Forwards args to the binary.
run-macos *args:
    cargo run -p naos-macos -- {{args}}

# =============================================================================
# Cross-crate
#
# These attempt to operate on every crate in the workspace. On any single
# host, at least one per-crate invocation will fail — naos-linux cannot
# compile on macOS, naos-macos cannot compile on Linux. Useful in CI with
# a matrix of hosts, less useful locally.
# =============================================================================

# Typecheck every crate. Will fail on the host-wrong one.
check-all:
    cargo check -p naos-linux
    cargo check -p naos-macos

# Format all Rust code in the workspace.
# Works on any host — cargo fmt does not compile anything.
fmt:
    cargo fmt --all

# Verify formatting without modifying. Useful for pre-commit or CI.
fmt-check:
    cargo fmt --all -- --check

# =============================================================================
# Test kernels
#
# Build the guest kernels used as naos test inputs. See DEVELOPMENT.md for
# prerequisites and the one-time setup required before these work.
# =============================================================================

# Build the x86_64 vmlinux used by naos-linux tests.
# Assumes the Linux kernel source is cloned to ~/src/linux.
kernel-linux:
    ./scripts/build-test-kernel-x86_64.sh

# Build the aarch64 Image used by naos-macos tests.
# Placeholder — real implementation lands with stage 2.
kernel-macos:
    ./scripts/build-test-kernel-aarch64.sh

# =============================================================================
# Debugging helpers
# =============================================================================

# Strace KVM ioctls during a naos-linux run. Linux only.
#
# The gold standard for seeing what ioctls the VMM is issuing when
# something goes wrong in boot.rs or the vCPU loop.
strace-linux kernel="testdata/vmlinux":
    strace -e ioctl cargo run -p naos-linux -- --kernel {{kernel}} --mem 256

# Print how just sees the current OS. Sanity check for the host-aware recipes.
os:
    @echo "just sees os = {{os()}}"
