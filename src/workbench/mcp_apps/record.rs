use crate::{Error, Result};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io, path::Path};
pub(in crate::workbench) fn hash(v: &Value) -> String {
    format!("{:x}", Sha256::digest(v.to_string().as_bytes()))
}
pub(in crate::workbench) fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::File::open(path) {
        Ok(f) => Ok(Some(serde_json::from_reader(f)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn put<T: Serialize>(path: &Path, value: &T, initial: bool) -> Result<bool> {
    let dir = path.parent().expect("application state directory");
    fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut file, value)?;
    file.as_file().sync_all()?;
    if initial {
        match file.persist_noclobber(path) {
            Ok(_) => {}
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(e.error.into()),
        }
    } else {
        file.persist(path).map_err(|e| e.error)?;
    }
    fs::File::open(dir)?.sync_all()?;
    Ok(true)
}
pub(in crate::workbench) fn capture<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    put(path, value, true).map(drop)
}
pub(in crate::workbench) fn reserve(path: &Path, key: &str) -> Result<Option<Value>> {
    if put(path, &json!({"request":key}), true)? {
        return Ok(None);
    }
    let v: Value = read(path)?.ok_or_else(|| Error::Protocol("lost MCP tool receipt".into()))?;
    if v["request"] != key {
        return Err(Error::Refused(
            "MCP command ID was used for another call".into(),
        ));
    }
    if let Some(r) = v.get("result") {
        return Ok(Some(r.clone()));
    }
    if let Some(e) = v["error"].as_str() {
        return Err(Error::Refused(e.to_owned()));
    }
    Err(Error::Refused("the original MCP call is still running or its outcome is unknown; check the server before issuing another command".into()))
}
pub(in crate::workbench) fn finish(path: &Path, key: &str, value: Result<Value>) -> Result<Value> {
    match value {
        Ok(v) => {
            put(path, &json!({"request":key,"result":v}), false)?;
            Ok(v)
        }
        Err(e) => {
            put(path, &json!({"request":key,"error":e.to_string()}), false)?;
            Err(e)
        }
    }
}
