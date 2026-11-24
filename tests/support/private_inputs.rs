use std::{fs, path::Path};

pub(crate) fn verify_private_inputs(dir: &Path, probes: &[String]) {
    for e in fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let kind = e.file_type().unwrap();
        if kind.is_dir() {
            verify_private_inputs(&e.path(), probes);
        } else if kind.is_file() {
            let bytes = fs::read(e.path()).unwrap();
            for value in probes {
                assert!(
                    !bytes
                        .windows(value.len())
                        .any(|slice| slice == value.as_bytes()),
                    "credential persisted in {}",
                    e.path().display()
                );
            }
        }
    }
}
