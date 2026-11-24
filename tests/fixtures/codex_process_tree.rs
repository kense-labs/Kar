// A real exec_command or MCP parent. Each child leaves on fixture teardown.
use std::os::unix::process::CommandExt;
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let marker = args[1].to_str().unwrap();
    let directory = Path::new(&args[2]).canonicalize().unwrap();
    let mut children: Vec<Child> = Vec::new();
    if marker == "mcp" {
        children.push(Command::new(&args[3]).arg("mcp").spawn().unwrap());
    }
    fs::write(directory.join(marker), std::process::id().to_string()).unwrap();
    let mut forked = false;
    let deadline = Instant::now() + Duration::from_secs(70);
    while directory.is_dir() && deadline > Instant::now() {
        if !forked && !marker.ends_with("-child") && directory.join("fork-now").is_file() {
            let mut cmd = Command::new(std::env::current_exe().unwrap());
            cmd.arg(format!("{marker}-child"))
                .arg(&directory)
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            children.push(cmd.spawn().unwrap());
            forked = true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    for mut child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
}
