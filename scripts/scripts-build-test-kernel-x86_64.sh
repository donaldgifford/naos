#!/bin/bash
#
# build-test-kernel-x86_64.sh
#
# Build the x86_64 test kernel used by naos-linux. The output vmlinux is
# symlinked into testdata/vmlinux at the naos repo root. See DEVELOPMENT.md
# for the one-time setup (kernel source clone, build dependencies) this
# script assumes is in place.
#
# This script is idempotent — run it again after pulling new kernel changes
# and it rebuilds only what changed.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
readonly REPO_ROOT="${SCRIPT_DIR}/.."
readonly KERNEL_SRC="${NAOS_KERNEL_SRC:-${HOME}/src/linux}"
readonly TESTDATA_DIR="${REPO_ROOT}/testdata"
readonly VMLINUX_SYMLINK="${TESTDATA_DIR}/vmlinux"

err() {
  echo "[$(date +'%Y-%m-%dT%H:%M:%S%z')]: $*" >&2
}

die() {
  err "$*"
  exit 1
}

check_prerequisites() {
  [[ -d "${KERNEL_SRC}" ]] \
    || die "Kernel source not found at ${KERNEL_SRC}. See DEVELOPMENT.md for setup. Override the path with NAOS_KERNEL_SRC."

  [[ -f "${KERNEL_SRC}/Makefile" ]] \
    || die "${KERNEL_SRC} does not look like a kernel source tree (no Makefile)."

  for cmd in make gcc flex bison bc; do
    command -v "${cmd}" >/dev/null 2>&1 \
      || die "Required build tool not found: ${cmd}. See DEVELOPMENT.md for the full list."
  done
}

configure_kernel() {
  pushd "${KERNEL_SRC}" >/dev/null

  # Start from the smallest possible config.
  make tinyconfig >/dev/null

  # Enable just enough to boot under naos-linux and print to serial. Each
  # option has a specific reason — see DEVELOPMENT.md for the rationale.
  ./scripts/config \
    --enable 64BIT \
    --enable PRINTK \
    --enable EARLY_PRINTK \
    --enable TTY \
    --enable SERIAL_8250 \
    --enable SERIAL_8250_CONSOLE \
    --enable BINFMT_ELF

  # Resolve any new dependencies tinyconfig did not pull in.
  make olddefconfig >/dev/null

  popd >/dev/null
}

build_kernel() {
  pushd "${KERNEL_SRC}" >/dev/null

  local jobs
  jobs="$(nproc 2>/dev/null || echo 4)"

  err "Building vmlinux (jobs=${jobs})..."
  make -j"${jobs}" vmlinux

  popd >/dev/null
}

wire_symlink() {
  mkdir -p "${TESTDATA_DIR}"

  # Use an atomic rename so in-progress builds never see a broken symlink.
  local tmp_link
  tmp_link="$(mktemp -u "${TESTDATA_DIR}/vmlinux.XXXXXX")"

  ln -s "${KERNEL_SRC}/vmlinux" "${tmp_link}"
  mv -f "${tmp_link}" "${VMLINUX_SYMLINK}"

  err "Symlinked ${VMLINUX_SYMLINK} -> ${KERNEL_SRC}/vmlinux"
}

main() {
  check_prerequisites
  configure_kernel
  build_kernel
  wire_symlink
  err "Done. Run \`just run\` to boot it."
}

main "$@"
