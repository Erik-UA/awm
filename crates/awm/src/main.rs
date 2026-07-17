//! `awm` binary entry point. The runtime is wired up in Phase 3; for now this is
//! a placeholder that reports build status so the workspace has a green bin.

fn main() {
    println!(
        "awm {} — scaffold only (contracts frozen at Phase 1; runtime lands in Phase 3). core: {}",
        env!("CARGO_PKG_VERSION"),
        awm_core::PHASE
    );
}
