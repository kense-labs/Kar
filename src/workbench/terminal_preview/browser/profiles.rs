//! Live reports from connected clients and the last persistently reported catalog.
use super::profile;
use crate::{Error, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Mutex};
use uuid::Uuid;
type Scope = (Uuid, Option<Uuid>);
#[derive(Clone, Serialize, Deserialize)]
pub(in super::super) struct Profile {
    id: String,
    name: String,
    kind: Kind,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Persistent,
    Incognito,
}
#[derive(Clone, Serialize, Deserialize)]
pub(in super::super) struct Catalog {
    profiles: Vec<Profile>,
    pub default_profile_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recording_frame_rate: Option<u8>,
}
impl Default for Catalog {
    fn default() -> Self {
        Self {
            profiles: vec![
                Profile {
                    id: "default".into(),
                    name: "Default".into(),
                    kind: Kind::Persistent,
                },
                Profile {
                    id: "incognito".into(),
                    name: "Incognito".into(),
                    kind: Kind::Incognito,
                },
            ],
            default_profile_id: "default".into(),
            recording_frame_rate: None,
        }
    }
}
impl Catalog {
    fn validate(&self) -> Result<()> {
        profile(&self.default_profile_id)?;
        if self.profiles.len() > 24
            || self
                .recording_frame_rate
                .is_some_and(|rate| !matches!(rate, 30 | 60))
        {
            return Err(Error::Refused("unsupported preview profile report".into()));
        }
        for row in &self.profiles {
            profile(&row.id)?;
            if row.name.trim() != row.name || row.name.is_empty() || row.name.chars().count() > 48 {
                return Err(Error::Refused("invalid browser profile name".into()));
            }
        }
        Ok(())
    }
}
struct Room {
    reports: IndexMap<Uuid, Catalog>,
    snapshot: Catalog,
}
impl Room {
    fn rebuild(&mut self, rate: Option<u8>) {
        let Some((_, last)) = self.reports.last() else {
            return;
        };
        let mut catalog = Catalog::default();
        let mut custom = IndexMap::new();
        for report in self.reports.values() {
            for profile in &report.profiles {
                if !matches!(profile.id.as_str(), "default" | "incognito") {
                    custom.insert(
                        profile.id.clone(),
                        Profile {
                            kind: Kind::Persistent,
                            ..profile.clone()
                        },
                    );
                }
            }
        }
        catalog.profiles.extend(custom.into_values());
        if catalog
            .profiles
            .iter()
            .any(|r| r.id == last.default_profile_id)
        {
            catalog.default_profile_id = last.default_profile_id.clone();
        }
        catalog.recording_frame_rate = rate.or(self.snapshot.recording_frame_rate);
        self.snapshot = catalog;
    }
}
pub(in super::super) struct Reports {
    dir: PathBuf,
    rooms: Mutex<HashMap<Scope, Room>>,
}
impl Reports {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            rooms: Default::default(),
        }
    }
    fn path(&self, scope: Scope) -> PathBuf {
        self.dir
            .join(scope.0.to_string())
            .join(scope.1.map_or_else(|| "local".into(), |s| s.to_string()))
            .join("reports.json")
    }
    fn read(&self, scope: Scope) -> Result<Catalog> {
        let path = self.path(scope);
        match std::fs::File::open(path) {
            Ok(file) => Ok(serde_json::from_reader(file)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Catalog::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn snapshot(&self, scope: Scope) -> Result<Catalog> {
        if let Some(room) = self.rooms.lock().unwrap().get(&scope) {
            return Ok(room.snapshot.clone());
        }
        self.read(scope)
    }
    pub fn report(&self, scope: Scope, client: Uuid, catalog: Catalog) -> Result<Catalog> {
        catalog.validate()?;
        let mut rooms = self.rooms.lock().unwrap();
        let room = match rooms.entry(scope) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => e.insert(Room {
                reports: IndexMap::new(),
                snapshot: self.read(scope)?,
            }),
        };
        let rate = catalog.recording_frame_rate;
        let mut next = Room {
            reports: room.reports.clone(),
            snapshot: room.snapshot.clone(),
        };
        next.reports.shift_remove(&client);
        next.reports.insert(client, catalog);
        next.rebuild(rate);
        let path = self.path(scope);
        std::fs::create_dir_all(path.parent().unwrap())?;
        super::super::write_json(&path, &next.snapshot)?;
        *room = next;
        Ok(room.snapshot.clone())
    }
    pub fn disconnect(&self, scope: Scope, client: Uuid) {
        if let Some(room) = self.rooms.lock().unwrap().get_mut(&scope) {
            room.reports.shift_remove(&client);
            room.rebuild(None);
        }
    }
}
