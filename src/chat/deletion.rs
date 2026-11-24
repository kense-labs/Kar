//! Retirement keeps a durable UUID reservation. Descendants may still need a
//! parent's native turn evidence and shared Git object database after retirement.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

fn absent_ok(result: std::io::Result<()>) -> Result<()> {
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => Ok(other?),
    }
}

impl ChatStore {
    fn deleted_marker(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.deleted"))
    }

    pub(super) fn require_available_id(&self, id: Uuid) -> Result<()> {
        if self.deleted_marker(id).try_exists()? {
            return Err(Error::Refused(
                "this chat ID was permanently deleted".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn ancestry_file(&self, id: Uuid) -> std::io::Result<File> {
        match File::open(self.path(id)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                File::open(self.dir.join(format!("{id}.ancestry")))
            }
            other => other,
        }
    }

    /// Called while the workbench holds the family's controller and stop fences.
    pub(crate) fn retire_chats(
        &self,
        deleted: &BTreeMap<Uuid, Uuid>,
        retiring: &BTreeSet<Uuid>,
    ) -> Result<()> {
        // A completed rewind can leave a workspace lease pointing to its reservation.
        // Removing that file allows the existing lease recovery to clear its own marker.
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "karrewind") {
                continue;
            }
            if let Some((chat, command)) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.split_once('.'))
                && Uuid::parse_str(command).is_ok()
                && Uuid::parse_str(chat).is_ok_and(|id| retiring.contains(&id))
            {
                absent_ok(fs::remove_file(path))?;
            }
        }
        for id in retiring {
            let marker = self.deleted_marker(*id);
            if !marker.try_exists()? {
                File::create_new(marker)?.sync_all()?;
            }
            // A rename makes the live view disappear without losing metadata needed after a crash.
            let src = self.path(*id);
            if src.try_exists()? {
                fs::rename(src, self.dir.join(format!("{id}.ancestry")))?;
            }
        }
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;

        let mut needed = BTreeSet::new();
        let mut parents: Vec<_> = self
            .list()?
            .iter()
            .filter_map(|i| i.forked_from.as_ref().map(|f| f.chat))
            .collect();
        while let Some(id) = parents.pop() {
            if !needed.insert(id) {
                continue;
            }
            match self.ancestry_file(id) {
                Ok(file) => {
                    if let Some(f) = log::info(file, id)?.forked_from {
                        parents.push(f.chat);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        let disposable: BTreeSet<_> = deleted
            .keys()
            .copied()
            .filter(|id| !needed.contains(id))
            .collect();
        for id in retiring {
            for path in [
                self.inbox_path(*id),
                self.dir.join(format!("{id}.context-fork")),
                self.dir.join(format!("{id}.fork-request")),
            ] {
                absent_ok(fs::remove_file(path))?;
            }
            let schema = self.dir.join("workbench/run-schemas").join(id.to_string());
            absent_ok(fs::remove_dir_all(schema))?;
        }
        for id in &disposable {
            absent_ok(fs::remove_file(self.dir.join(format!("{id}.ancestry"))))?;
            absent_ok(fs::remove_dir_all(self.dir.join(format!("{id}.git"))))?;
        }
        self.remove_retired_attachments(&disposable)?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}
