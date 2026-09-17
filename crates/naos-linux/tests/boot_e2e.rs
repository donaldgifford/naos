//! End-to-end smoke test: boot the bundled test kernel and confirm naos-linux
//! reaches the documented success signal (the init panic) and exits cleanly.
//!
//! This exercises the parts no unit test can reach on its own — `Vmm::new`, the
//! vCPU run loop, serial output, and the reset-driven clean exit — by running
//! the built binary against a real kernel.
//!
//! It requires two things that are not always present:
//!   * read/write access to `/dev/kvm` (root, or membership in the `kvm` group)
//!   * a built test kernel at `testdata/vmlinux` (run `just kernel-linux`)
//!
//! When either is missing the test skips, so `cargo test` stays green in
//! environments without KVM. To run it for real here:
//!     `sudo -E cargo test -p naos-linux --test boot_e2e -- --nocapture`

use std::path::PathBuf;
use std::process::Command;

/// `testdata/vmlinux` at the repository root, relative to this crate.
fn kernel_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/vmlinux")
}

/// Whether this process can actually open `/dev/kvm`.
fn kvm_accessible() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

#[test]
fn boots_to_init_panic_and_exits_zero() {
    let kernel = kernel_path();
    if !kvm_accessible() || !kernel.exists() {
        eprintln!(
            "skipping boot_e2e: needs /dev/kvm access and a kernel at {}",
            kernel.display()
        );
        return;
    }

    // Wrap in `timeout` so a future regression that hangs the guest fails the
    // test (exit 124) instead of blocking the suite forever.
    let output = Command::new("timeout")
        .arg("60")
        .arg(env!("CARGO_BIN_EXE_naos-linux"))
        .args(["--kernel", kernel.to_str().unwrap(), "--mem", "256"])
        .output()
        .expect("failed to spawn naos-linux");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected a clean exit, got {:?}\n--- stdout ---\n{stdout}",
        output.status
    );
    assert!(
        stdout.contains("Linux version"),
        "kernel banner missing from output"
    );
    assert!(
        stdout.contains("No working init found"),
        "kernel did not reach the init-panic success signal"
    );
}
