//! Presenter window/runtime test. Gated behind `TL_E2E=1` (SPEC §9) because
//! it opens a real AppKit window and renders to it.
//!
//! AppKit's main-thread requirement conflicts with the test harness (tests
//! run on spawned threads), so the actual runtime check is the
//! `present_demo` example, which this test launches as a subprocess:
//!
//!     TL_E2E=1 cargo test -p tl-macos-render --test presenter_e2e

#[test]
fn presenter_window_runtime() {
    if std::env::var("TL_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipped: set TL_E2E=1 to run the windowed presenter E2E");
        return;
    }
    let status = std::process::Command::new(env!("CARGO"))
        .args(["run", "-q", "-p", "tl-macos-render", "--example", "present_demo"])
        .status()
        .expect("spawn present_demo example");
    assert!(status.success(), "present_demo failed: {status}");
}
