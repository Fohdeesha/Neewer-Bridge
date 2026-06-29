//! Neewer-Bridge binary entry point.
//!
//! Milestone 1 stub: real CLI (clap), config, ArtNet listener and BLE manager
//! are added in subsequent milestones. For now this just confirms the crate
//! builds and links against the library.

fn main() {
    println!(
        "neewer-bridge {} — protocol core only (no BLE/ArtNet yet)",
        env!("CARGO_PKG_VERSION")
    );
}
