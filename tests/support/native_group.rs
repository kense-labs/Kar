use nix::{
    sys::signal::{Signal, killpg},
    unistd::{Pid, getpgid},
};
use std::time::Duration;

pub(crate) struct NativeGroup(Pid);
impl NativeGroup {
    pub(crate) fn of(controller: u32) -> Self {
        let children = std::process::Command::new("pgrep")
            .args(["-P", &controller.to_string()])
            .output()
            .unwrap();
        assert!(
            children.status.success(),
            "Kar controller has no native child"
        );
        let pid = String::from_utf8(children.stdout).unwrap();
        let pid = Pid::from_raw(pid.trim().parse().expect("one native process group"));
        assert_eq!(getpgid(Some(pid)).unwrap(), pid);
        Self(pid)
    }
    pub(crate) fn signal(&self, signal: Signal) {
        killpg(self.0, signal).unwrap();
    }
}
impl Drop for NativeGroup {
    fn drop(&mut self) {
        let _ = killpg(self.0, Signal::SIGKILL);
    }
}

pub(crate) async fn wait(mut ready: impl FnMut() -> bool, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}
