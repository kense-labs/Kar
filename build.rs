use std::{env, ffi::OsString, path::PathBuf, process::Command};

fn main() {
    for file in [
        "node_api",
        "pipe",
        "dsh_skills",
        "dsh_models",
        "dsh_images",
        "dsh_profile",
        "dsh_agent_controls",
        "dsh_agent_startup",
        "dsh_prompt",
        "dsh_title",
        "pi_tools",
        "pi_mcp",
        "pi_requests",
        "pi_auth",
        "pi_catalog",
        "pi_picker",
    ] {
        println!("cargo:rerun-if-changed=native/{file}.rs");
    }
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    if !matches!(os.as_str(), "linux" | "macos" | "windows") {
        return;
    }
    // Embed the target libraries so cargo install and SDK consumers ship the same adapters.
    for (source, library) in [
        ("dsh_skills", "dsh-skills"),
        ("dsh_profile", "dsh-profile"),
        ("dsh_images", "dsh-images"),
        ("pi_tools", "pi-tools"),
        ("pi_mcp", "pi-mcp"),
        ("pi_requests", "pi-requests"),
        ("pi_auth", "pi-auth"),
    ] {
        build_plugin(&os, source, library);
    }
}
fn build_plugin(os: &str, source: &str, library: &str) {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join(format!("{library}.node"));
    let mut rustc = Command::new(env::var_os("RUSTC").unwrap());
    rustc
        .args(["--edition=2024", "--crate-type=cdylib", "--target"])
        .arg(env::var_os("TARGET").unwrap())
        .args(["-Copt-level=s", "-Cpanic=abort", "-Cdebuginfo=0"]);
    if let Some(linker) = env::var_os("RUSTC_LINKER") {
        let mut option = OsString::from("linker=");
        option.push(linker);
        rustc.arg("-C").arg(option);
    }
    // Honor the caller's target SDK, library search paths and linker flags.
    if let Ok(flags) = env::var("CARGO_ENCODED_RUSTFLAGS") {
        rustc.args(flags.split('\x1f').filter(|f| !f.is_empty()));
    }
    if os == "macos" {
        rustc.args(["-Clink-arg=-undefined", "-Clink-arg=dynamic_lookup"]);
    }
    let status = rustc
        .arg(format!("native/{source}.rs"))
        .arg("-o")
        .arg(out)
        .status()
        .expect("launch rustc for the native inventory plugin");
    assert!(status.success(), "could not build the {library} plugin");
}
