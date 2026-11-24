use super::*;
use std::collections::BTreeSet;
type Write = (Method, Url, Option<Value>);
fn patch(edits: &mut Vec<Write>, method: Method, url: Url, payload: Value) {
    edits.push((method, url, Some(payload)));
}
pub(super) async fn apply(p: &Pull<'_>, action: &Request) -> Result<Value, ForgeError> {
    let Request::Update {
        title,
        body,
        state,
        ready,
        add_labels,
        remove_labels,
        request_reviewers,
    } = action
    else {
        return Err(invalid("pull action"));
    };
    if title.as_ref().is_some_and(|v| v.trim().is_empty())
        || state
            .as_deref()
            .is_some_and(|s| !matches!(s, "open" | "closed"))
    {
        return Err(ForgeError::bad("invalid title or state"));
    }
    for list in [add_labels, remove_labels, request_reviewers]
        .into_iter()
        .flatten()
    {
        if list.is_empty()
            || list.len() > 100
            || list
                .iter()
                .any(|s| s.trim().is_empty() || s.len() > 256 || s == "." || s == "..")
        {
            return Err(ForgeError::bad("invalid label or reviewer"));
        }
    }
    if add_labels
        .iter()
        .flatten()
        .any(|s| remove_labels.as_ref().is_some_and(|xs| xs.contains(s)))
    {
        return Err(ForgeError::bad("conflicting label operations"));
    }
    let mut changes = vec![];
    let mut fields = json!({});
    let kind = p.c.kind.as_str();
    if let Some(title) = title {
        fields["title"] = json!(title);
    }
    if let Some(body) = body {
        fields[if matches!(kind, "gitlab" | "gitcafe" | "azure-devops" | "bitbucket") {
            "description"
        } else {
            "body"
        }] = json!(body);
    }
    if let Some(open) = state.as_deref().map(|s| s == "open") {
        match kind {
            "gitlab" => fields["state_event"] = json!(if open { "reopen" } else { "close" }),
            "azure-devops" => fields["status"] = json!(if open { "active" } else { "abandoned" }),
            "gitcafe" => patch(
                &mut changes,
                Method::POST,
                p.endpoint(&[if open { "reopen" } else { "close" }]),
                json!({}),
            ),
            "bitbucket" if open => return Err(p.unsupported("reopen")),
            "bitbucket" => changes.push((Method::POST, p.endpoint(&["decline"]), None)),
            _ => fields["state"] = json!(if open { "open" } else { "closed" }),
        }
    }
    if let Some(ready) = ready {
        match kind {
            "cursor-origin" => fields["draft"] = json!(!ready),
            "azure-devops" => fields["isDraft"] = json!(!ready),
            "gitlab" => {
                let title = fields["title"]
                    .as_str()
                    .or(p.raw["title"].as_str())
                    .ok_or_else(|| invalid("GitLab merge request title"))?;
                let trimmed = title.trim_start();
                let plain = ["draft:", "[draft]", "(draft)", "wip:", "[wip]", "(wip)"]
                    .iter()
                    .find_map(|pre| {
                        trimmed
                            .get(..pre.len())
                            .filter(|s| s.eq_ignore_ascii_case(pre))
                            .map(|_| trimmed[pre.len()..].trim_start())
                    })
                    .unwrap_or(trimmed);
                fields["title"] = json!(if *ready {
                    plain.to_owned()
                } else {
                    format!("Draft: {plain}")
                });
            }
            "tanstack-source" => changes.push((
                if *ready { Method::PUT } else { Method::DELETE },
                p.endpoint(&["ready"]),
                None,
            )),
            "gitcafe" => patch(
                &mut changes,
                Method::POST,
                p.endpoint(&[if *ready { "ready" } else { "draft" }]),
                json!({}),
            ),
            _ => return Err(p.unsupported("ready/draft")),
        }
    }
    if add_labels.is_some() || remove_labels.is_some() {
        let url = child(p.repo.clone(), &["issues", &p.n.to_string(), "labels"]);
        match kind {
            "gitlab" => {
                for (field, labels) in
                    [("add_labels", add_labels), ("remove_labels", remove_labels)]
                {
                    if let Some(names) = labels {
                        if names.iter().any(|s| s.contains(',')) {
                            return Err(ForgeError::bad(
                                "GitLab label names cannot contain a comma",
                            ));
                        }
                        fields[field] = json!(names.join(","));
                    }
                }
            }
            "forgejo" => {
                if let Some(names) = add_labels {
                    patch(
                        &mut changes,
                        Method::POST,
                        url.clone(),
                        json!({"labels":issue::labels(p.c,&p.repo,names).await?}),
                    );
                }
                if let Some(names) = remove_labels {
                    for id in issue::labels(p.c, &p.repo, names).await? {
                        changes.push((
                            Method::DELETE,
                            child(url.clone(), &[&id.to_string()]),
                            None,
                        ));
                    }
                }
            }
            "tanstack-source" => {
                if let Some(names) = add_labels {
                    patch(
                        &mut changes,
                        Method::POST,
                        url.clone(),
                        json!({"labels":names}),
                    );
                }
                for s in remove_labels.iter().flatten() {
                    changes.push((Method::DELETE, child(url.clone(), &[s]), None));
                }
            }
            "gitcafe" => {
                let labels = cafe_pages(p.c, child(p.repo.clone(), &["labels"])).await?;
                let current = cafe_pages(p.c, p.endpoint(&["labels"])).await?;
                let mut ids = cafe_ids(&current)?;
                for (add, list) in [(true, add_labels), (false, remove_labels)] {
                    for name in list.iter().flatten() {
                        let id = labels
                            .iter()
                            .find(|l| l["name"].as_str() == Some(name))
                            .and_then(|l| l["id"].as_str())
                            .ok_or_else(|| {
                                ForgeError::bad(format!("unknown GitCafe label {name}"))
                            })?;
                        if add {
                            ids.insert(id.to_owned());
                        } else {
                            ids.remove(id);
                        }
                    }
                }
                patch(
                    &mut changes,
                    Method::PUT,
                    p.endpoint(&["labels"]),
                    json!({"labelIds":ids}),
                );
            }
            _ => return Err(p.unsupported("labels")),
        }
    }
    if let Some(users) = request_reviewers {
        match kind {
            "gitlab" => {
                let mut ids = BTreeSet::new();
                for r in p.raw["reviewers"]
                    .as_array()
                    .ok_or_else(|| invalid("GitLab reviewers"))?
                {
                    ids.insert(number(&r["id"])?);
                }
                ids.extend(issue::gitlab_users(p.c, users).await?);
                fields["reviewer_ids"] = json!(ids);
            }
            "bitbucket" => {
                let mut ids = BTreeSet::new();
                for r in p.raw["reviewers"]
                    .as_array()
                    .ok_or_else(|| invalid("Bitbucket reviewers"))?
                {
                    ids.insert(
                        r["uuid"]
                            .as_str()
                            .ok_or_else(|| invalid("Bitbucket reviewer id"))?
                            .to_owned(),
                    );
                }
                ids.extend(users.iter().cloned());
                fields["reviewers"] = json!(
                    ids.into_iter()
                        .map(|v| json!({"uuid":v}))
                        .collect::<Vec<_>>()
                );
            }
            "azure-devops" => {
                for id in users {
                    uuid::Uuid::parse_str(id)
                        .map_err(|_| ForgeError::bad("Azure reviewer must be an identity UUID"))?;
                    patch(
                        &mut changes,
                        Method::PUT,
                        p.endpoint(&["reviewers", id]),
                        json!({"id":id}),
                    );
                }
            }
            "forgejo" => patch(
                &mut changes,
                Method::POST,
                p.endpoint(&["requested_reviewers"]),
                json!({"reviewers":users}),
            ),
            "tanstack-source" => patch(
                &mut changes,
                Method::POST,
                p.endpoint(&["reviewers"]),
                json!({"add":users}),
            ),
            "gitcafe" => {
                let current = cafe_pages(p.c, p.endpoint(&["reviewers"])).await?;
                let mut ids = cafe_ids(&current)?;
                ids.extend(users.iter().cloned());
                patch(
                    &mut changes,
                    Method::PUT,
                    p.endpoint(&["reviewers"]),
                    json!({"actorIds":ids}),
                );
            }
            _ => return Err(p.unsupported("requesting reviewers")),
        }
    }
    if fields.as_object().is_some_and(|obj| !obj.is_empty()) {
        changes.insert(
            0,
            (
                if matches!(kind, "bitbucket" | "gitlab") {
                    Method::PUT
                } else {
                    Method::PATCH
                },
                p.url.clone(),
                Some(fields),
            ),
        );
    }
    if changes.is_empty() {
        return Err(ForgeError::bad("no pull update requested"));
    }
    let mut version = p.raw["version"].as_u64();
    for (i, (method, url, mut payload)) in changes.into_iter().enumerate() {
        if kind == "gitcafe" {
            let version = version.ok_or_else(|| {
                if i == 0 {
                    invalid("GitCafe version")
                } else {
                    ForgeError::uncertain("GitCafe did not return the version after updating")
                }
            })?;
            payload.get_or_insert_with(|| json!({}))["expectedVersion"] = json!(version);
        }
        let response = send(p.c, method, url, payload, true).await.map_err(|e| {
            if i == 0 {
                e
            } else {
                ForgeError::uncertain(format!("{i} pull changes accepted before {}", e.detail))
            }
        })?;
        version = response["version"].as_u64().or_else(|| {
            response
                .pointer("/pullRequest/version")
                .and_then(Value::as_u64)
        });
    }
    Ok(json!({"number":p.n,"updated":true}))
}
fn cafe_ids(list: &[Value]) -> Result<BTreeSet<String>, ForgeError> {
    list.iter()
        .map(|v| {
            v["id"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("GitCafe list id"))
        })
        .collect()
}
pub(super) async fn cafe_pages(c: &Context, base: Url) -> Result<Vec<Value>, ForgeError> {
    let mut url = base.clone();
    let mut rows = vec![];
    let mut cursors = BTreeSet::new();
    for _ in 0..1000 {
        url.query_pairs_mut().append_pair("limit", "100");
        let data = get(c, url).await?;
        rows.extend(
            data["items"]
                .as_array()
                .ok_or_else(|| invalid("GitCafe list"))?
                .iter()
                .cloned(),
        );
        let Some(cursor) = data["next"].as_str().filter(|s| !s.is_empty()) else {
            return Ok(rows);
        };
        if !cursors.insert(cursor.to_owned()) {
            break;
        }
        url = base.clone();
        url.query_pairs_mut().append_pair("after", cursor);
    }
    Err(invalid("GitCafe list cursor"))
}
