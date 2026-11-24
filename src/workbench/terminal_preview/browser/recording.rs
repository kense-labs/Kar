//! Write Chromium's actual lossy WebP keyframes into a timestamped, video-only WebM.
use super::*;
use tokio::{sync::oneshot, task::JoinHandle, time::Instant};
#[derive(Default)]
pub(super) struct Recordings(AsyncMutex<HashMap<Uuid, Live>>);
struct Live {
    started: String,
    stop: oneshot::Sender<()>,
    worker: JoinHandle<Result<Vec<u8>>>,
}
const MAX: usize = 32 * 1024 * 1024;
fn invalid() -> Error {
    Error::Protocol("Chromium did not return a supported VP8 video frame".into())
}
fn frame(bytes: &[u8]) -> Result<(&[u8], u16, u16)> {
    if bytes.get(..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WEBP") {
        return Err(invalid());
    }
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let tag = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let data = bytes.get(pos + 8..pos + 8 + len).ok_or_else(invalid)?;
        if tag == b"VP8 "
            && data.len() >= 10
            && data[0] & 1 == 0
            && data[3..6] == [0x9d, 0x01, 0x2a]
        {
            let w = u16::from_le_bytes(data[6..8].try_into().unwrap()) & 0x3fff;
            let h = u16::from_le_bytes(data[8..10].try_into().unwrap()) & 0x3fff;
            if w > 0 && h > 0 {
                return Ok((data, w, h));
            }
        }
        pos += 8 + len + (len & 1);
    }
    Err(invalid())
}
fn element(dst: &mut Vec<u8>, id: u32, value: &[u8]) {
    let bytes = id.to_be_bytes();
    let i = bytes.iter().position(|b| *b != 0).unwrap();
    dst.extend_from_slice(&bytes[i..]);
    let n = (1..=8)
        .find(|n| (value.len() as u64) < (1u64 << (7 * n)) - 1)
        .unwrap();
    let encoded = ((1u64 << (7 * n)) | value.len() as u64).to_be_bytes();
    dst.extend_from_slice(&encoded[8 - n..]);
    dst.extend_from_slice(value);
}
fn integer(dst: &mut Vec<u8>, id: u32, num: u64) {
    let b = num.to_be_bytes();
    let i = b.iter().position(|b| *b != 0).unwrap_or(7);
    element(dst, id, &b[i..]);
}
fn header(w: u16, h: u16) -> Vec<u8> {
    let mut file = vec![];
    let mut ebml = vec![];
    for (k, v) in [(0x4286, 1), (0x42f7, 1), (0x42f2, 4), (0x42f3, 8)] {
        integer(&mut ebml, k, v);
    }
    element(&mut ebml, 0x4282, b"webm");
    integer(&mut ebml, 0x4287, 2);
    integer(&mut ebml, 0x4285, 2);
    element(&mut file, 0x1a45dfa3, &ebml);
    file.extend_from_slice(&[
        0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    ]);
    let mut info = vec![];
    integer(&mut info, 0x2ad7b1, 1_000_000);
    element(&mut info, 0x4d80, b"Kar");
    element(&mut info, 0x5741, b"Kar");
    element(&mut file, 0x1549a966, &info);
    let mut video = vec![];
    integer(&mut video, 0xb0, w.into());
    integer(&mut video, 0xba, h.into());
    let mut track = vec![];
    integer(&mut track, 0xd7, 1);
    integer(&mut track, 0x73c5, 1);
    integer(&mut track, 0x83, 1);
    element(&mut track, 0x86, b"V_VP8");
    element(&mut track, 0xe0, &video);
    let mut tracks = vec![];
    element(&mut tracks, 0xae, &track);
    element(&mut file, 0x1654ae6b, &tracks);
    file
}
fn append(video: &mut Vec<u8>, webp: &[u8], ms: u64) -> Result<()> {
    let (key, w, h) = frame(webp)?;
    if video.is_empty() {
        *video = header(w, h);
    }
    if video.len() + key.len() + 40 > MAX {
        return Err(Error::Refused("recording exceeds 32 MiB".into()));
    }
    let mut simple = vec![0x81, 0x00, 0x00, 0x80];
    simple.extend_from_slice(key);
    let mut cluster = vec![];
    integer(&mut cluster, 0xe7, ms);
    element(&mut cluster, 0xa3, &simple);
    element(video, 0x1f43b675, &cluster);
    Ok(())
}
impl Pool {
    pub(in super::super) async fn record_start(
        self: &Arc<Self>,
        grant: Arc<Grant>,
        check: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    ) -> Result<Value> {
        let id = grant.info.id;
        let mut all = self.recordings.0.lock().await;
        all.retain(|_, r| !r.worker.is_finished());
        if let Some(r) = all.get(&id) {
            return Ok(json!({"tab_id":id,"recording":true,"started_at":r.started}));
        }
        if all.len() >= 4 {
            return Err(Error::Busy);
        }
        let started = jiff::Timestamp::now().to_string();
        let at = Instant::now();
        let mut video = vec![];
        append(&mut video, &self.recording_frame(&grant).await?, 0)?;
        let (stop, mut ending) = oneshot::channel();
        let pool = Arc::downgrade(self);
        let mut revoked = grant.revocation();
        let worker = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _=&mut ending=>break,
                    _=revoked.changed()=>{
                        if let Some(pool)=pool.upgrade(){pool.recordings.0.lock().await.remove(&id);}
                        return Err(Error::Closed("preview closed during recording".into()));
                    }
                    _=tokio::time::sleep(Duration::from_millis(200))=>{
                        let pool=pool.upgrade().ok_or_else(||Error::Closed("preview service stopped".into()))?;
                        check()?;
                        let image=pool.recording_frame(&grant).await?;
                        append(&mut video,&image,at.elapsed().as_millis() as u64)?;
                    }
                }
            }
            Ok(video)
        });
        all.insert(
            id,
            Live {
                started: started.clone(),
                stop,
                worker,
            },
        );
        Ok(json!({"tab_id":id,"recording":true,"started_at":started}))
    }
    pub(in super::super) async fn record_stop(
        &self,
        grant: Arc<Grant>,
    ) -> Result<(Vec<u8>, String)> {
        let r = self
            .recordings
            .0
            .lock()
            .await
            .remove(&grant.info.id)
            .ok_or_else(|| Error::Refused("there is no recording for this tab".into()))?;
        let _ = r.stop.send(());
        Ok((
            r.worker.await.map_err(|e| Error::Closed(e.to_string()))??,
            r.started,
        ))
    }
}
