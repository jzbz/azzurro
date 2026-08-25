//! The binary is a wrapper: everything it does lives in the library beside it,
//! so the parts worth benchmarking can be reached from `benches/`.

// Windows gives a program a console unless it is told otherwise, so a release
// build opened from the Start menu came up with a terminal window sitting
// behind it — `file` on the first artifact the packaging workflow produced
// said "(console)" outright.
//
// Only in release. The subsystem that hides the window also takes stdout and
// stderr with it, and a debug build is exactly when someone wants to see what
// `RUST_LOG` has to say. Ignored on every other platform, so no `cfg` around
// it: Linux and macOS decide this from the desktop entry and the bundle.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    azzurro_gui::run_app()
}
