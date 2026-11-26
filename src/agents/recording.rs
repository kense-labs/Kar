use crate::{Error, Result, Session};
use serde_json::json;
use std::path::Path;
use tokio::{
    fs::{File, OpenOptions},
    io::AsyncWriteExt,
    task::JoinHandle,
};

pub(super) struct Recorder(JoinHandle<Result<()>>);

async fn append(file: &mut File, value: &impl serde::Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    file.write_all(&bytes).await?;
    file.flush().await?;
    file.sync_data().await?;
    Ok(())
}

impl Recorder {
    pub async fn start(path: &Path, runtime: &str, session: &Session) -> Result<Self> {
        let mut stream = session.subscribe(None)?;
        let mut open = OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        open.mode(0o600);
        let mut file = open.open(path).await?;
        append(
            &mut file,
            &json!({
                "kind": "header",
                "format": "kar-voyage/3",
                "recordEncoding": "kar-record/1",
                "recorder": "kar",
                "runtime": runtime,
                "cwd": session.cwd(),
                "sessionId": session.id(),
                "streamId": session.stream_id(),
                "startedAt": crate::stream::timestamp_ms(),
            }),
        )
        .await?;
        Ok(Self(tokio::spawn(async move {
            #[derive(serde::Serialize)]
            struct Entry<'a> {
                kind: &'static str,
                record: &'a crate::Record,
            }
            let mut records = 0u64;
            while let Some(record) = stream.next().await {
                append(
                    &mut file,
                    &Entry {
                        kind: "record",
                        record: &record,
                    },
                )
                .await?;
                records += 1;
            }
            append(
                &mut file,
                &json!({
                    "kind": "end",
                    "at": crate::stream::timestamp_ms(),
                    "reason": "closed",
                    "records": records,
                }),
            )
            .await?;
            file.sync_all().await?;
            Ok(())
        })))
    }

    pub async fn finish(self) -> Result<()> {
        self.0
            .await
            .map_err(|e| Error::Protocol(format!("recording task failed: {e}")))?
    }

    pub fn abort(&self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn append_flushes_and_syncs_every_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.voyage.jsonl");
        let stream_id = uuid::Uuid::new_v4();
        let mut open = OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        open.mode(0o600);
        let mut file = open.open(&path).await.unwrap();
        append(
            &mut file,
            &json!({
                "kind": "header",
                "format": "kar-voyage/3",
                "recordEncoding": "kar-record/1",
                "streamId": stream_id,
                "sessionId": "s",
            }),
        )
        .await
        .unwrap();
        drop(file);
        let summary = crate::voyage::import(
            std::io::BufReader::new(std::fs::File::open(&path).unwrap()),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(summary.stream_id, stream_id);
        assert_eq!(summary.records, 0);
        assert!(summary.end.is_none());
    }
}
