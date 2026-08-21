//! The binary is a wrapper: everything it does lives in the library beside it,
//! so the parts worth benchmarking can be reached from `benches/`.

fn main() -> std::process::ExitCode {
    azzurro_gui::run_app()
}
