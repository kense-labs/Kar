use super::*;
use std::collections::{BTreeMap, HashMap, HashSet};

fn id(v: &Value) -> Result<String, ForgeError> {
    if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
        Ok(s.into())
    } else {
        Ok(number(v)?.to_string())
    }
}
fn comment(r: &Value, author: &Value, body: &Value) -> Result<Value, ForgeError> {
    let name = author.as_str().or_else(|| {
        [
            "username",
            "handle",
            "login",
            "display_name",
            "slug",
            "nickname",
            "name",
        ]
        .into_iter()
        .find_map(|key| author[key].as_str())
    });
    Ok(json!({"id":id(&r["id"])?,"author":name,"body":body.as_str().unwrap_or("")}))
}
fn entry(id: &str, path: &Value, resolved: bool) -> Value {
    json!({"id":id,"path":path,"resolved":resolved,"comments":[]})
}
fn append(t: &mut Value, comment: Value) {
    t["comments"]
        .as_array_mut()
        .expect("constructed comment collection")
        .push(comment);
}
async fn raw(c: &Context, root: &Url) -> Result<Vec<Value>, ForgeError> {
    let collection = match c.kind.as_str() {
        "gitlab" => "discussions",
        "forgejo" => "reviews",
        _ => "comments",
    };
    let base = child(root.clone(), &[collection]);
    let mut next: Option<String> = None;
    let mut seen = HashSet::new();
    let mut all = vec![];
    for page in 1..=100 {
        let mut u = base.clone();
        match c.kind.as_str() {
            "gitlab" | "forgejo" => {
                u.query_pairs_mut()
                    .append_pair(
                        if c.kind == "gitlab" {
                            "per_page"
                        } else {
                            "limit"
                        },
                        "50",
                    )
                    .append_pair("page", &page.to_string());
            }
            "bitbucket" => {
                if let Some(s) = &next {
                    u = Url::parse(s).map_err(|_| invalid("Bitbucket comment cursor"))?;
                    if u.origin() != base.origin()
                        || u.path() != base.path()
                        || !u.username().is_empty()
                        || u.password().is_some()
                    {
                        return Err(invalid("Bitbucket comment cursor scope"));
                    }
                } else {
                    u.query_pairs_mut().append_pair("pagelen", "50");
                }
            }
            "cursor-origin" | "gitcafe" => {
                let (limit, cursor) = if c.kind == "gitcafe" {
                    ("limit", "after")
                } else {
                    ("pageSize", "pageToken")
                };
                u.query_pairs_mut().append_pair(limit, "50");
                if let Some(s) = &next {
                    u.query_pairs_mut().append_pair(cursor, s);
                }
            }
            _ => {}
        }
        let data = get(c, u).await?;
        let rows = match c.kind.as_str() {
            "bitbucket" => &data["values"],
            "cursor-origin" => &data["comments"],
            "gitcafe" => &data["items"],
            _ => &data,
        }
        .as_array()
        .ok_or_else(|| invalid("pull discussion list"))?;
        all.extend_from_slice(rows);
        match c.kind.as_str() {
            "gitlab" | "forgejo" if rows.len() == 50 => {}
            "bitbucket" | "gitcafe" | "cursor-origin" => {
                next = data[if c.kind == "cursor-origin" {
                    "nextPageToken"
                } else {
                    "next"
                }]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
                let Some(s) = &next else { return Ok(all) };
                if !seen.insert(s.clone()) {
                    return Err(invalid("repeated review pagination cursor"));
                }
            }
            _ => return Ok(all),
        }
    }
    Err(invalid("pull discussion list exceeded 100 pages"))
}

pub(super) async fn read(c: &Context, root: &Url) -> Result<Value, ForgeError> {
    let rows = raw(c, root).await?;
    let mut groups = BTreeMap::new();
    let by_id: HashMap<_, _> = rows
        .iter()
        .filter_map(|r| r["id"].as_u64().map(|n| (n, r)))
        .collect();
    for r in &rows {
        match c.kind.as_str() {
            "gitlab" => {
                let notes = r["notes"]
                    .as_array()
                    .ok_or_else(|| invalid("GitLab discussion notes"))?;
                let Some(first) = notes
                    .iter()
                    .find(|v| v["position"].is_object() || v["resolvable"] == true)
                else {
                    continue;
                };
                let key = id(&r["id"])?;
                let mut group = entry(
                    &key,
                    &first["position"]["new_path"],
                    notes
                        .iter()
                        .filter(|r| r["resolvable"] == true)
                        .all(|r| r["resolved"] == true),
                );
                for note in notes.iter().filter(|n| n["system"] != true) {
                    append(&mut group, comment(note, &note["author"], &note["body"])?);
                }
                groups.insert(key, group);
            }
            "forgejo" => {
                if r["state"] == "PENDING" || r["comments_count"].as_u64().unwrap_or(0) == 0 {
                    continue;
                }
                let comments = get(
                    c,
                    child(root.clone(), &["reviews", &id(&r["id"])?, "comments"]),
                )
                .await?;
                for row in comments
                    .as_array()
                    .ok_or_else(|| invalid("Forgejo review comments"))?
                {
                    let key = id(&row["id"])?;
                    let mut t = entry(&key, &row["path"], false);
                    append(&mut t, comment(row, &row["user"], &row["body"])?);
                    groups.insert(key, t);
                }
            }
            "tanstack-source" => {
                let key = id(&r["id"])?;
                let mut t = entry(&key, &r["path"], r["resolved"] == true);
                for row in r["comments"]
                    .as_array()
                    .ok_or_else(|| invalid("Source thread comments"))?
                {
                    append(&mut t, comment(row, &row["author"], &row["comment"])?);
                }
                groups.insert(key, t);
            }
            "bitbucket" => {
                let mut root = *by_id
                    .get(&number(&r["id"])?)
                    .ok_or_else(|| invalid("Bitbucket comment ID"))?;
                let mut seen = HashSet::new();
                while let Some(parent) = root
                    .pointer("/parent/id")
                    .and_then(Value::as_u64)
                    .and_then(|id| by_id.get(&id))
                {
                    if !seen.insert(number(&root["id"])?) {
                        return Err(invalid("cycle in review comments"));
                    }
                    root = parent;
                }
                if !root["inline"]["path"].is_string() {
                    continue;
                }
                let key = id(&root["id"])?;
                let t = groups.entry(key.clone()).or_insert_with(|| {
                    entry(&key, &root["inline"]["path"], !root["resolution"].is_null())
                });
                if r["deleted"] != true {
                    append(t, comment(r, &r["user"], &r["content"]["raw"])?);
                }
            }
            "cursor-origin" => {
                let thread = &r["thread"];
                if !thread["path"].is_string() {
                    continue;
                }
                let key = id(&thread["id"])?;
                let t = groups.entry(key.clone()).or_insert_with(|| {
                    entry(&key, &thread["path"], !thread["resolvedAt"].is_null())
                });
                append(t, comment(r, &r["author"], &r["body"])?);
            }
            "gitcafe" => {
                let key = id(&r["threadId"])?;
                let t = groups
                    .entry(key.clone())
                    .or_insert_with(|| entry(&key, &Value::Null, false));
                if id(&r["id"])? == key {
                    t["version"] = r["version"].clone();
                    t["resolved"] = (!r["resolvedAt"].is_null()).into();
                    t["path"] = r["path"].clone();
                }
                append(t, comment(r, &r["author"], &r["body"])?);
            }
            _ => unreachable!(),
        }
    }
    Ok(json!({"threads":groups.into_values().collect::<Vec<_>>(),"nextCursor":null}))
}
pub(super) async fn find(c: &Context, root: &Url, key: &str) -> Result<Value, ForgeError> {
    let page = read(c, root).await?;
    page["threads"]
        .as_array()
        .and_then(|v| v.iter().find(|t| t["id"] == key))
        .cloned()
        .ok_or_else(|| ForgeError::remote(404, "thread does not belong to this pull request"))
}
