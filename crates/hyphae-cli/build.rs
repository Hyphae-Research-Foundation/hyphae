// SPDX-License-Identifier: GPL-3.0-only

//! Captures the build compiler identity for hardware calibration receipts.

use std::{env, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=RUSTC");
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let identity = Command::new(rustc)
        .args(["--version", "--verbose"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().replace('\n', "; "))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "rustc identity unavailable".to_owned());
    println!("cargo:rustc-env=HYPHAE_RUSTC_IDENTITY={identity}");
}
