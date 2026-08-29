//! Puts the pinned default theme where `rust-embed` will find it.
//!
//! The hub embeds a built theme, and `scripts/theme.sh` is what fetches one.
//! Running it here means a plain `cargo build` needs no setup step and cannot
//! be pointed at the wrong theme by accident: the build derives its own input
//! from `web-theme.pin` instead of trusting a directory somebody maintained by
//! hand. See that script for why the directory is gone.

use std::process::Command;

fn main() {
    // Cargo reruns this only when one of these changes, so an untouched pin
    // costs nothing. The stamp is listed too: delete `target/theme` and the
    // fetch happens again rather than failing later inside rust-embed.
    println!("cargo:rerun-if-changed=web-theme.pin");
    println!("cargo:rerun-if-changed=target/theme/.pin");
    println!("cargo:rerun-if-changed=scripts/theme.sh");

    match Command::new("sh").arg("scripts/theme.sh").status() {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("scripts/theme.sh failed ({status}); the message above says why"),
        Err(e) => panic!("could not run scripts/theme.sh: {e}"),
    }
}
