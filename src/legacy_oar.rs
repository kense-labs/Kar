//! Compatibility entry point for existing command-line scripts.
#![forbid(unsafe_code)]

#[path = "main.rs"]
mod cli;

fn main() -> std::process::ExitCode {
    cli::main()
}
