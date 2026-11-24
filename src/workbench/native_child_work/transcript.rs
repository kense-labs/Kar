use super::{
    Child,
    profile::{self, Profile},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, Read, Seek, SeekFrom};
#[derive(Deserialize)]
struct Step {
    step_index: u64,
    #[serde(rename = "type")]
    kind: String,
    status: Option<String>,
    content: Option<String>,
    error: Option<String>,
    #[serde(default)]
    tool_calls: Vec<Call>,
}
#[derive(Deserialize)]
struct Call {
    name: String,
    args: Option<Value>,
}
fn short(text: &str, n: usize) -> String {
    if text.encode_utf16().count() <= n {
        return text.into();
    }
    let units = text.encode_utf16().take(n).collect::<Vec<_>>();
    format!("{}…", String::from_utf16_lossy(&units))
}
pub(super) fn read(p: &Profile, c: &Child, budget: usize) -> io::Result<(Vec<Value>, bool)> {
    let parts = ["brain", &c.id, ".system_generated", "logs"];
    let mut f = profile::file(&p.dir(&parts)?, "transcript.jsonl")?;
    if let Some(uri) = &c.uri {
        let expected = p.path.join(parts.join("/")).join("transcript.jsonl");
        let named = url::Url::parse(uri)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .and_then(|p| p.canonicalize().ok());
        if named.as_ref() != Some(&expected) {
            return Err(profile::invalid());
        }
    }
    let size = f.metadata()?.len();
    let start = size.saturating_sub(budget as u64);
    let at = start.saturating_sub(1);
    f.seek(SeekFrom::Start(at))?;
    let mut bytes = Vec::with_capacity((size - at) as usize);
    f.take(size - at).read_to_end(&mut bytes)?;
    let retained = if start > 0 {
        bytes.splitn(2, |b| *b == b'\n').nth(1).unwrap_or(&[])
    } else {
        &bytes
    };
    let steps=retained.split(|b|*b==b'\n').filter_map(|line|serde_json::from_slice::<Step>(line).ok()).filter_map(|step|{
        let prompt=c.launch.prompt.trim();
        if step.step_index==0 && step.kind=="SYSTEM_MESSAGE" && !prompt.is_empty() && step.content.as_ref().is_some_and(|s|s.contains(prompt)){return None;}
        let tools=step.tool_calls.into_iter().map(|call|{
            let mut args=call.args.unwrap_or_else(||json!({}));
            if let Some(map)=args.as_object_mut(){for value in map.values_mut(){if let Some(decoded)=value.as_str().and_then(|s|serde_json::from_str(s).ok()){*value=decoded;}}}
            json!({"name":call.name,"input":short(&args.to_string(),1000)})
        }).collect::<Vec<_>>();
        Some(json!({"index":step.step_index,"type":step.kind,"status":step.status,"text":step.content.or(step.error).map(|s|short(&s,4000)),"tool_calls":tools}))
    }).collect();
    Ok((steps, start > 0))
}
