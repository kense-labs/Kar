//! Compatibility entry point for existing scripts invoking `oar`.
#![forbid(unsafe_code)]

#[path = "main.rs"]
mod cli;

fn main() -> std::process::ExitCode {
    cli::main()
}
