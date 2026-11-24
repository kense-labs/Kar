use super::*;
use std::io::Read;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Query {
    number: u64,
    include: Option<Vec<String>>,
    include_resolved: Option<bool>,
    cursor: Option<String>,
    limit: Option<usize>,
}
#[derive(Serialize, Deserialize)]
struct Cursor {
    sig: String,
    section: String,
    offset: usize,
    page: usize,
    token: Option<String>,
    resolved: bool,
}
async fn comments(
    c: &Context,
    n: u64,
    cur: &Cursor,
) -> Result<(Vec<Value>, bool, Option<String>), ForgeError> {
    let i = n.to_string();
    let r = repo(c)?;
    let k = c.kind.as_str();
    let mut url = match k {
        "gitlab" => child(r, &["merge_requests", &i, "notes"]),
        "azure-devops" => child(r, &["pullrequests", &i, "threads"]),
        "bitbucket" => child(r, &["pullrequests", &i, "comments"]),
        "tanstack-source" => child(r, &["issues", &i]),
        "gitcafe" | "cursor-origin" => child(r, &["pulls", &i, "comments"]),
        _ => child(r, &["issues", &i, "comments"]),
    };
    {
        let mut q = url.query_pairs_mut();
        match k {
            "gitcafe" | "cursor-origin" | "tanstack-source" => {
                q.append_pair(
                    if k == "cursor-origin" {
                        "pageSize"
                    } else {
                        "limit"
                    },
                    "50",
                );
                if let Some(s) = &cur.token {
                    q.append_pair(
                        match k {
                            "gitcafe" => "after",
                            "cursor-origin" => "pageToken",
                            _ => "cursor",
                        },
                        s,
                    );
                }
            }
            "azure-devops" => {}
            _ => {
                q.append_pair(
                    match k {
                        "forgejo" => "limit",
                        "bitbucket" => "pagelen",
                        _ => "per_page",
                    },
                    "50",
                )
                .append_pair("page", &(cur.page + 1).to_string());
            }
        }
    }
    let raw = providers::get(c, url).await?;
    let list = match k {
        "azure-devops" => &raw["value"],
        "bitbucket" => &raw["values"],
        "tanstack-source" => &raw["timeline"],
        "gitcafe" => &raw["items"],
        "cursor-origin" => &raw["comments"],
        _ => &raw,
    };
    let empty = Vec::new();
    let rows = if k == "cursor-origin" && list.is_null() {
        &empty
    } else {
        list.as_array()
            .ok_or_else(|| ForgeError::bad("invalid PR comment page"))?
    };
    let mut out = Vec::new();
    fn add(out: &mut Vec<Value>, r: &Value, body: &Value) {
        if r["id"].is_null()
            || !body.is_string()
            || r["isDeleted"] == true
            || r["deleted"] == true
            || r["system"] == true
        {
            return;
        }
        let author = r
            .get("author")
            .or(r.get("user"))
            .cloned()
            .unwrap_or_default();
        out.push(json!({"id":r["id"],"body":body,"author":author,"url":r.get("html_url").or(r.get("url"))}));
    }
    for r in rows {
        match k {
            "azure-devops" if r["threadContext"].is_null() => {
                for row in r["comments"].as_array().into_iter().flatten() {
                    add(&mut out, row, &row["content"]);
                    if let Some(last) = out.last_mut() {
                        last["thread_id"] = r["id"].clone();
                    }
                }
            }
            "azure-devops" => {}
            "tanstack-source" => add(&mut out, r, &r["comment"]),
            "bitbucket" if r["inline"].is_null() => add(&mut out, r, &r["content"]["raw"]),
            "bitbucket" => {}
            _ if r["position"].is_null()
                && r["path"].is_null()
                && r["thread"]["path"].is_null() =>
            {
                add(&mut out, r, &r["body"])
            }
            _ => {}
        }
    }
    let token = raw[if k == "cursor-origin" {
        "nextPageToken"
    } else {
        "next"
    }]
    .as_str()
    .filter(|s| !s.is_empty())
    .map(str::to_owned);
    let more = match k {
        "azure-devops" | "forgejo" => false,
        "bitbucket" | "gitcafe" | "cursor-origin" | "tanstack-source" => token.is_some(),
        _ => rows.len() == 50,
    };
    Ok((out, more, if k == "bitbucket" { None } else { token }))
}
async fn rows(
    c: &Context,
    n: u64,
    cur: &mut Cursor,
    limit: usize,
) -> Result<(Vec<Value>, bool), ForgeError> {
    let mut out = vec![];
    for _ in 0..50 {
        let (rows, more, token) = if cur.section == "threads" {
            let page = c
                .execute(&Operation::ReviewThreads {
                    number: n,
                    cursor: cur.token.clone(),
                })
                .await?;
            let threads = page["threads"]
                .as_array()
                .ok_or_else(|| ForgeError::bad("invalid PR threads"))?
                .iter()
                .filter(|r| cur.resolved || r["resolved"] != true)
                .cloned()
                .collect();
            let token = page["nextCursor"].as_str().map(str::to_owned);
            (threads, token.is_some(), token)
        } else {
            comments(c, n, cur).await?
        };
        let take = rows.len().saturating_sub(cur.offset).min(limit - out.len());
        out.extend(rows.iter().skip(cur.offset).take(take).cloned());
        cur.offset += take;
        if cur.offset < rows.len() {
            return Ok((out, true));
        }
        if !more {
            return Ok((out, false));
        }
        if token.is_some() && token == cur.token {
            return Err(ForgeError::bad("repeated pull discussion cursor"));
        }
        cur.page += 1;
        cur.offset = 0;
        cur.token = token;
        if out.len() == limit {
            return Ok((out, true));
        }
    }
    Ok((out, true))
}
fn diff(
    c: &Context,
    n: u64,
    pull: &Value,
    offset: usize,
    size: usize,
) -> Result<(Value, usize, bool), ForgeError> {
    let (base, head) = review::fetch(c, n, pull)?;
    let mut process = crate::workbench_git::command(&c.cwd)
        .args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            &format!("{base}...{head}"),
            "--",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| ForgeError::bad(e.to_string()))?;
    let mut pipe = process.stdout.take().unwrap();
    let result = (|| -> std::io::Result<_> {
        if std::io::copy(&mut (&mut pipe).take(offset as u64), &mut std::io::sink())?
            != offset as u64
        {
            return Err(std::io::Error::other("invalid diff cursor"));
        }
        let mut bytes = Vec::new();
        (&mut pipe).take(size as u64 + 1).read_to_end(&mut bytes)?;
        Ok(bytes)
    })();
    let more = result.as_ref().is_ok_and(|b| b.len() > size);
    drop(pipe);
    if more || result.is_err() {
        let _ = process.kill();
    }
    let status = process.wait().map_err(|e| ForgeError::bad(e.to_string()))?;
    let mut bytes = result.map_err(|e| ForgeError::bad(e.to_string()))?;
    if !more && !status.success() {
        return Err(ForgeError::bad("git diff failed at the selected revisions"));
    }
    bytes.truncate(size);
    if more
        && let Err(e) = std::str::from_utf8(&bytes)
        && e.error_len().is_none()
    {
        bytes.truncate(e.valid_up_to());
    }
    let count = bytes.len();
    let v = match String::from_utf8(bytes) {
        Ok(s) => json!({"text":s}),
        Err(e) => {
            json!({"bytes_base64":base64::engine::general_purpose::STANDARD.encode(e.into_bytes())})
        }
    };
    Ok((v, count, more))
}
pub(super) async fn read(
    app: &Workbench,
    p: Uuid,
    c: &Context,
    q: Query,
) -> Result<Value, ForgeError> {
    let limit = q.limit.unwrap_or(30);
    if q.number == 0 || !(1..=100).contains(&limit) {
        return Err(ForgeError::bad("invalid pull number or read limit"));
    }
    let mut detail = review::refresh(app, p, c, q.number).await?;
    let sig = signature(
        c,
        &json!([q.number, detail["head_sha"], detail["base_sha"]]),
    );
    let sections = if let Some(cursor) = q.cursor {
        let cur: Cursor = decode(&cursor)?;
        if q.include.is_some()
            || q.include_resolved.is_some()
            || cur.sig != sig
            || cur.page > 1_000_000
            || cur.offset > usize::try_from(u32::MAX).unwrap_or(usize::MAX)
        {
            return Err(ForgeError::bad("stale or mismatched pull read cursor"));
        }
        vec![cur]
    } else {
        q.include
            .unwrap_or_default()
            .into_iter()
            .map(|section| Cursor {
                sig: sig.clone(),
                section,
                offset: 0,
                page: 0,
                token: None,
                resolved: q.include_resolved.unwrap_or(false),
            })
            .collect()
    };
    let mut seen = std::collections::HashSet::new();
    for mut cursor in sections {
        let section = cursor.section.clone();
        if !matches!(section.as_str(), "diff" | "threads" | "comments")
            || !seen.insert(section.clone())
        {
            return Err(ForgeError::bad("unknown or duplicated read section"));
        }
        let (mut frame, more) = if section == "diff" {
            let context = c.clone();
            let data = detail.clone();
            let offset = cursor.offset;
            let (data, count, more) = tokio::task::spawn_blocking(move || {
                diff(&context, q.number, &data, offset, limit * 1024)
            })
            .await
            .map_err(|e| ForgeError::bad(e.to_string()))??;
            cursor.offset += count;
            (data, more)
        } else {
            let (rows, more) = rows(c, q.number, &mut cursor, limit).await?;
            (json!({"items":rows}), more)
        };
        frame["next_cursor"] = if more {
            json!(encode(&cursor)?)
        } else {
            Value::Null
        };
        detail[&section] = frame;
    }
    Ok(detail)
}
