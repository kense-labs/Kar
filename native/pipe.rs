use std::io::{self, Write};

pub(super) fn stdout(mut pending: &[u8]) {
    let mut out = io::stdout().lock();
    // Large catalogs can exceed the nonblocking pipe's capacity.
    while !pending.is_empty() {
        match out.write(pending) {
            Ok(0) => return,
            Ok(n) => pending = &pending[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(2))
            }
            Err(_) => return,
        }
    }
    loop {
        match out.flush() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(2))
            }
            _ => return,
        }
    }
}
