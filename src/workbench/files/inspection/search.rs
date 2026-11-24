use super::*;
use regex::{Regex, RegexBuilder};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    sync::LazyLock,
};

fn default_limit() -> usize {
    100
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Kind {
    File,
    Directory,
}
#[derive(Deserialize)]
pub(super) struct EntryQuery {
    #[serde(default)]
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    kind: Option<Kind>,
    #[serde(default)]
    image_only: bool,
}
fn image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|x| x.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "avif"
                | "bmp"
                | "gif"
                | "ico"
                | "heic"
                | "heif"
                | "jpeg"
                | "jpg"
                | "png"
                | "svg"
                | "tif"
                | "tiff"
                | "webp"
                | "icns"
        )
    )
}
// Prefer exact component matches, followed by contiguous and then ordered character matches.
fn rank(path: &Path, query: &str) -> Option<(u8, usize)> {
    if query.is_empty() {
        return Some((0, 0));
    }
    let path = path.to_string_lossy().to_lowercase();
    let name = path.rsplit('/').next().unwrap_or_default();
    if name == query {
        return Some((0, 0));
    }
    if name.starts_with(query) {
        return Some((1, name.len() - query.len()));
    }
    if let Some(i) = path.find(query) {
        return Some((2, i));
    }
    let mut offset = 0;
    let mut start = 0;
    for (i, c) in query.chars().enumerate() {
        let pos = offset + path[offset..].find(c)?;
        if i == 0 {
            start = pos;
        }
        offset = pos + c.len_utf8();
    }
    Some((3, offset - start))
}
pub(super) async fn entries(
    State(app): State<Workbench>,
    Param(id): Param<Uuid>,
    Query(q): Query<EntryQuery>,
) -> Reply {
    blocking(move || {
        if !(1..=200).contains(&q.limit) || q.query.chars().count() > 256 {
            return Err(Error::Refused("invalid path search limit or query".into()));
        }
        let ws = app.workspace(&app.project(id)?)?;
        let query = q.query.trim().to_lowercase();
        let mut winners = BTreeMap::new();
        let mut truncated = false;
        ws.visit_indexed(|e| {
            let allowed = match q.kind {
                Some(Kind::File) => e.kind == "file",
                Some(Kind::Directory) => e.kind == "directory",
                None => true,
            };
            if allowed
                && (!q.image_only || e.kind == "file" && image(&e.path))
                && let Some(priority) = rank(&e.path, &query)
            {
                winners.insert((priority, e.path.clone()), e);
                if winners.len() > q.limit {
                    winners.pop_last();
                    truncated = true;
                }
            }
            Ok(true)
        })?;
        Ok(json!({"entries":winners.into_values().collect::<Vec<_>>(),"truncated":truncated}))
    })
    .await
}
#[derive(Deserialize)]
pub(super) struct ContentQuery {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    whole_word: bool,
    #[serde(default)]
    use_regex: bool,
}
fn word(c: Option<char>) -> bool {
    static MARK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\p{M}$").unwrap());
    c.is_some_and(|c| c == '_' || c.is_alphanumeric() || MARK.is_match(c.encode_utf8(&mut [0; 4])))
}
fn whole(line: &str, start: usize, end: usize) -> bool {
    start != end
        && !(word(line[..start].chars().next_back()) && word(line[start..].chars().next()))
        && !(word(line[..end].chars().next_back()) && word(line[end..].chars().next()))
}
pub(super) async fn contents(
    State(app): State<Workbench>,
    Param(id): Param<Uuid>,
    Query(q): Query<ContentQuery>,
) -> Reply {
    blocking(move||{
        if !(1..=500).contains(&q.limit) || q.query.is_empty() || q.query.chars().count()>256{return Err(Error::Refused("invalid content search limit or query".into()));}
        let compile=|s:&str|RegexBuilder::new(s).case_insensitive(!q.case_sensitive).build();
        let literal=regex::escape(&q.query);
        let (regex,fallback)=match compile(if q.use_regex{&q.query}else{&literal}){
            Ok(r)=>(r,None),
            Err(e)=>(compile(&literal).map_err(|e|Error::Refused(e.to_string()))?,Some(e.to_string())),
        };
        let ws=app.workspace(&app.project(id)?)?;
        let mut matches=Vec::new();
        ws.visit_indexed(|entry|{
            if entry.kind!="file"{return Ok(true);}
            let file=match ws.target(&entry.path,false).and_then(|real|ws.open_regular(&real)){
                Ok(file)=>file,
                Err(Error::Io(e)) if matches!(e.kind(),io::ErrorKind::NotFound|io::ErrorKind::PermissionDenied)=>return Ok(true),
                Err(Error::Refused(_))=>return Ok(true),
                Err(e)=>return Err(e),
            };
            let mut reader=BufReader::new(file);
            if reader.fill_buf()?.contains(&0){return Ok(true);}
            let mut bytes=Vec::new();
            let mut line_number=0;
            loop {
                bytes.clear();
                if reader.read_until(b'\n',&mut bytes)?==0{break;}
                line_number+=1;
                let Ok(raw)=std::str::from_utf8(&bytes) else {continue};
                let line=raw.trim_end_matches(['\r','\n']);
                let mut found=regex.find_iter(line).filter(|m|!q.whole_word||whole(line,m.start(),m.end())).peekable();
                if found.peek().is_none(){continue;}
                let mut utf16_len=0;
                let end=line.char_indices().find_map(|(i,c)|{utf16_len+=c.len_utf16();(utf16_len>512).then_some(i)}).unwrap_or(line.len());
                let mut ranges=Vec::new();
                for m in found.take_while(|m|m.end()<=end){
                    let start=line[..m.start()].encode_utf16().count();
                    ranges.push(json!({"start":start,"end":start+m.as_str().encode_utf16().count()}));
                }
                if q.whole_word && ranges.is_empty(){continue;}
                let path=crate::chat::checkpoint::git_path::serialize(&entry.path,serde_json::value::Serializer)?;
                matches.push(json!({"path":path,"line_number":line_number,"line_content":&line[..end],"match_ranges":ranges}));
                if matches.len()>q.limit{return Ok(false);}
            }
            Ok(true)
        })?;
        let truncated=matches.len()>q.limit;
        matches.truncate(q.limit);
        let mut result=json!({"matches":matches,"truncated":truncated});
        if let Some(e)=fallback{result["regex_fallback_error"]=e.into();}
        Ok(result)
    }).await
}
