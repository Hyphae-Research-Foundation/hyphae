// SPDX-License-Identifier: Apache-2.0

//! Captures the exact Rust compilation target in the execution profile.

fn main() {
    println!("cargo:rerun-if-env-changed=TARGET");
    if let Ok(target) = std::env::var("TARGET") {
        println!("cargo:rustc-env=HYPHAE_BUILD_TARGET={target}");
    }
}
