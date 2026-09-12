//! Puts the pinned default theme where `rust-embed` will find it.
//!
//! The hub embeds a built theme, which `scripts/theme.sh` fetches. Running it
//! here lets a plain `cargo build` proceed without a setup step, taking its
//! input from `web-theme.pin` rather than a hand-maintained directory.

use std::process::Command;

fn main() {
    // Cargo reruns this only when one of these changes. The stamp is listed as
    // well, so deleting `target/theme` triggers a refetch rather than a later
    // failure inside rust-embed.
    println!("cargo:rerun-if-changed=web-theme.pin");
    println!("cargo:rerun-if-changed=target/theme/.pin");
    println!("cargo:rerun-if-changed=scripts/theme.sh");

    match Command::new("sh").arg("scripts/theme.sh").status() {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("scripts/theme.sh failed ({status}); see the message above"),
        Err(e) => panic!("could not run scripts/theme.sh: {e}"),
    }
}
