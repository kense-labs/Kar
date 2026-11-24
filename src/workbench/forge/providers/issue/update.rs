use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Fields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignees: Option<Vec<String>>,
}
impl Fields {
    pub(in crate::workbench::forge) fn valid(&self) -> bool {
        *self != Self::default()
            && self
                .title
                .as_deref()
                .is_none_or(|t| !t.is_empty() && t == t.trim() && t.encode_utf16().count() <= 1024)
            && self
                .body
                .as_deref()
                .is_none_or(|b| b.encode_utf16().count() <= 65536)
            && [(&self.labels, 50), (&self.assignees, 25)]
                .iter()
                .all(|(v, n)| {
                    v.as_ref().is_none_or(|v| {
                        v.len() <= *n && v.iter().all(|s| !s.is_empty() && s.trim() == s)
                    })
                })
    }
    pub(in crate::workbench::forge) fn text(&self, body_key: &str) -> Value {
        let mut value = json!({});
        if let Some(t) = &self.title {
            value["title"] = json!(t)
        }
        if let Some(t) = &self.body {
            value[body_key] = json!(t)
        }
        value
    }
}
fn names(rows: &Value, key: &str) -> Option<Vec<String>> {
    rows.as_array()?
        .iter()
        .map(|v| Some(v.as_str().or_else(|| v[key].as_str())?.to_ascii_lowercase()))
        .collect()
}
fn equal(expected: &[String], actual: Option<Vec<String>>) -> bool {
    let Some(mut got) = actual else { return false };
    let mut want = expected
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect::<Vec<_>>();
    got.sort();
    want.sort();
    got == want
}
pub(super) fn matches(c: &Context, v: &Value, f: &Fields) -> bool {
    let lab = names(&v["labels"], "name");
    let assigned = names(
        &v["assignees"],
        if c.kind == "gitlab" {
            "username"
        } else {
            "login"
        },
    );
    f.title
        .as_ref()
        .is_none_or(|s| v["title"].as_str() == Some(s))
        && f.body.as_ref().is_none_or(|s| {
            v[if c.kind == "gitlab" {
                "description"
            } else {
                "body"
            }]
            .as_str()
            .unwrap_or("")
                == s
        })
        && f.labels.as_ref().is_none_or(|v| equal(v, lab))
        && f.assignees.as_ref().is_none_or(|v| equal(v, assigned))
}
pub(super) async fn apply(c: &Context, repo: Url, n: u64, f: &Fields) -> Result<Value, ForgeError> {
    let target = child(repo.clone(), &["issues", &n.to_string()]);
    verify_issue(c, &get(c, target.clone()).await?, n)?;
    let mut base = f.text(if c.kind == "gitlab" {
        "description"
    } else {
        "body"
    });
    let mut additional = Vec::new();
    match c.kind.as_str() {
        "github" => {
            if let Some(v) = &f.labels {
                base["labels"] = json!(v)
            }
            if let Some(v) = &f.assignees {
                base["assignees"] = json!(v)
            }
        }
        "gitlab" => {
            if let Some(v) = &f.labels {
                base["labels"] = json!(v.join(","))
            }
            if let Some(v) = &f.assignees {
                base["assignee_ids"] = json!(gitlab_users(c, v).await?)
            }
        }
        "forgejo" => {
            if let Some(v) = &f.labels {
                additional.push(("labels", json!({"labels":labels(c,&repo,v).await?})))
            }
            if let Some(v) = &f.assignees {
                base["assignees"] = json!(v)
            }
        }
        "tanstack-source" => {
            for (field, names) in [("labels", &f.labels), ("assignees", &f.assignees)] {
                if let Some(v) = names {
                    additional.push((field, json!({(field):v})))
                }
            }
        }
        _ => return Err(ForgeError::unsupported("this driver has no issue editor")),
    }
    let mut wrote = false;
    if base.as_object().is_some_and(|m| !m.is_empty()) {
        let verb = if c.kind == "gitlab" {
            Method::PUT
        } else {
            Method::PATCH
        };
        let result = send(c, verb, target.clone(), Some(base), true).await?;
        if c.kind == "github" {
            verify_issue(c, &result, n)
                .map_err(|_| ForgeError::uncertain("GitHub did not confirm the edited issue"))?;
            if !matches(c, &result, f) {
                return Err(ForgeError::uncertain(
                    "GitHub did not apply all issue fields; verify triage permissions and the remote issue",
                ));
            }
        }
        wrote = true;
    }
    for (field, body) in additional {
        send(
            c,
            Method::PUT,
            child(target.clone(), &[field]),
            Some(body),
            true,
        )
        .await
        .map_err(|e| {
            if wrote {
                ForgeError::uncertain(format!(
                    "issue #{n} was partially edited; {field} requires reconciliation"
                ))
            } else {
                e
            }
        })?;
        wrote = true;
    }
    Ok(json!({"number":n}))
}

pub(in crate::workbench::forge) async fn confirm(
    c: &Context,
    n: u64,
    f: &Fields,
) -> Result<Value, ForgeError> {
    let actual = match c.kind.as_str() {
        "azure-devops" => super::super::azure::issue_fields(c, n).await?,
        "linear" => super::super::linear::issue_fields(c, n).await?,
        "github" | "forgejo" | "gitlab" | "tanstack-source" => {
            let repo = if c.kind == "tanstack-source" {
                super::super::hosted::resource(c, None, None)?
            } else {
                super::super::rest::project(c)?
            };
            let raw = get(c, child(repo, &["issues", &n.to_string()])).await?;
            verify_issue(c, &raw, n)?;
            raw
        }
        _ => return Err(ForgeError::unsupported("issue editing is unavailable")),
    };
    // A user-facing assignee identifier may be a native ID, login or display name.
    // Match every requested identity to one distinct result (and reject extra assignees).
    let mut wanted = f.clone();
    if wanted.labels.is_some() && actual["_labels_truncated"] == true {
        return Err(ForgeError::uncertain("remote issue labels are truncated"));
    }
    if let Some(requested) = wanted.assignees.take() {
        let Some(assigned) = actual["assignees"].as_array() else {
            return Err(ForgeError::uncertain(
                "remote assignee directory unavailable",
            ));
        };
        let mut remaining: Vec<_> = assigned.iter().collect();
        for needle in requested {
            let Some(index) = remaining.iter().position(|row| {
                row.as_str()
                    .is_some_and(|v| v.eq_ignore_ascii_case(&needle))
                    || [
                        "id",
                        "username",
                        "login",
                        "uniqueName",
                        "displayName",
                        "email",
                        "name",
                    ]
                    .iter()
                    .any(|k| {
                        row[*k]
                            .as_str()
                            .is_some_and(|v| v.eq_ignore_ascii_case(&needle))
                            || row[*k].as_u64().is_some_and(|v| v.to_string() == needle)
                    })
            }) else {
                return Err(ForgeError::uncertain(
                    "remote issue assignees have not confirmed the edit",
                ));
            };
            remaining.remove(index);
        }
        if !remaining.is_empty() {
            return Err(ForgeError::uncertain(
                "remote issue has additional assignees",
            ));
        }
    }
    if !matches(c, &actual, &wanted) {
        return Err(ForgeError::uncertain(
            "remote issue has not confirmed the requested fields",
        ));
    }
    Ok(json!({"number":n}))
}
