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
fn inv_similarity(candidate: &str, query: &str) -> u16 {
    let sim = strsim::jaro_winkler(candidate, query).clamp(0.0, 1.0);
    ((1.0 - sim) * 10_000.0) as u16
}

fn boundary_subsequence_span(path: &str, query: &str) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    let mut start = 0usize;
    let mut non_boundary_hits = 0usize;
    for (i, c) in query.chars().enumerate() {
        let rel = path[offset..].find(c)?;
        let pos = offset + rel;
        if i == 0 {
            start = pos;
        }
        let at_boundary = pos == 0
            || matches!(
                path[..pos].chars().next_back(),
                Some('/' | '_' | '-' | '.' | ' ')
            );
        if !at_boundary {
            non_boundary_hits += 1;
        }
        offset = pos + c.len_utf8();
    }
    Some((offset - start, non_boundary_hits))
}

// Prefer exact filename matches, filename prefixes, filename/path substrings,
// ordered fuzzy subsequences, and high-similarity typo matches via `strsim`.
fn rank(path: &Path, query: &str) -> Option<(u8, usize, u16)> {
    if query.is_empty() {
        return Some((0, 0, 0));
    }
    let path_str = path.to_string_lossy().to_lowercase();
    let name = path_str.rsplit('/').next().unwrap_or_default();
    let stem = name.split('.').next().unwrap_or(name);
    let sim_penalty = inv_similarity(name, query).min(inv_similarity(stem, query));

    if name == query {
        return Some((0, 0, 0));
    }
    if name.starts_with(query) {
        return Some((1, name.len() - query.len(), sim_penalty));
    }
    if let Some(pos) = name.find(query) {
        return Some((2, pos, sim_penalty));
    }
    if let Some(pos) = path_str.find(query) {
        return Some((3, pos, sim_penalty));
    }
    if let Some((span, non_boundary)) = boundary_subsequence_span(&path_str, query) {
        return Some((4, span.saturating_add(non_boundary * 2), sim_penalty));
    }
    if query.chars().count() >= 4 {
        let best_jw = strsim::jaro_winkler(name, query).max(strsim::jaro_winkler(stem, query));
        if best_jw >= 0.86 {
            let penalty = ((1.0 - best_jw).clamp(0.0, 1.0) * 10_000.0) as u16;
            return Some((5, usize::from(penalty), penalty));
        }
    }
    None
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
        let (indexed, index_truncated) = ws.cached_indexed()?;
        let mut winners = BTreeMap::new();
        let mut truncated = index_truncated;
        for e in indexed.iter() {
            let allowed = match q.kind {
                Some(Kind::File) => e.kind == "file",
                Some(Kind::Directory) => e.kind == "directory",
                None => true,
            };
            if allowed
                && (!q.image_only || e.kind == "file" && image(&e.path))
                && let Some(priority) = rank(&e.path, &query)
            {
                winners.insert((priority, e.path.clone()), e.clone());
                if winners.len() > q.limit {
                    winners.pop_last();
                    truncated = true;
                }
            }
        }
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
        let (indexed,_)=ws.cached_indexed()?;
        let mut matches=Vec::new();
        'files: for entry in indexed.iter(){
            if entry.kind!="file"{continue;}
            let file=match ws.target(&entry.path,false).and_then(|real|ws.open_regular(&real)){
                Ok(file)=>file,
                Err(Error::Io(e)) if matches!(e.kind(),io::ErrorKind::NotFound|io::ErrorKind::PermissionDenied)=>continue,
                Err(Error::Refused(_))=>continue,
                Err(e)=>return Err(e),
            };
            let mut reader=BufReader::new(file);
            if reader.fill_buf()?.contains(&0){continue;}
            let mut bytes=Vec::new();
            let mut line_number=0;
            loop {
                bytes.clear();
                if reader.read_until(b'\n',&mut bytes)?==0{break;}
                line_number+=1;
                if bytes.contains(&0){break;}
                let decoded=match std::str::from_utf8(&bytes){
                    Ok(s)=>std::borrow::Cow::Borrowed(s),
                    Err(_)=>match encoding_rs::GB18030.decode_without_bom_handling_and_without_replacement(&bytes){
                        Some(s)=>s,
                        None=>continue,
                    },
                };
                let line=decoded.trim_end_matches(['\r','\n']);
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
                if matches.len()>q.limit{break 'files;}
            }
        }
        let truncated=matches.len()>q.limit;
        matches.truncate(q.limit);
        let mut result=json!({"matches":matches,"truncated":truncated});
        if let Some(e)=fallback{result["regex_fallback_error"]=e.into();}
        Ok(result)
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_rank_orders_exact_prefix_substring_subsequence_and_typo_matches() {
        let exact = rank(Path::new("src/persistence.rs"), "persistence.rs").unwrap();
        let prefix = rank(Path::new("src/persistence_wal.rs"), "persistence").unwrap();
        let path_sub = rank(Path::new("src/persistence/store.rs"), "persistence").unwrap();
        let subseq = rank(Path::new("src/workbench/persistence.rs"), "wbprs").unwrap();
        let typo = rank(Path::new("src/workbench/persistence.rs"), "persistance").unwrap();

        assert!(exact < prefix);
        assert!(prefix < path_sub);
        assert!(path_sub < subseq);
        assert!(subseq < typo);
        assert!(rank(Path::new("src/workbench/mod.rs"), "zzzz_nomatch").is_none());
    }
}
