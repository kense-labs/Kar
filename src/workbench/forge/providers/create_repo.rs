use super::*;
use crate::workbench::forge::repository::{CloneUrls, clone_urls};
use uuid::Uuid;

pub(in crate::workbench::forge) async fn create(
    c: &Context,
    private: bool,
    request_id: Uuid,
) -> Result<CloneUrls, ForgeError> {
    let visibility = if private { "private" } else { "public" };
    let (owner, name) = c
        .repository
        .rsplit_once('/')
        .map(|(o, n)| (Some(o), n))
        .unwrap_or((None, &c.repository));
    if c.kind != "gitlab" && c.kind != "azure-devops" && owner.is_some_and(|o| o.contains('/')) {
        return Err(ForgeError::bad("expected owner/name"));
    }
    let mut target = c.clone();
    let v = match c.kind.as_str() {
        "github" | "forgejo" => {
            let viewer = if owner.is_some() || c.kind == "forgejo" {
                Some(get(c, path(c, &["user"])?).await?)
            } else {
                None
            };
            let login = viewer
                .as_ref()
                .map(|v| {
                    v["login"]
                        .as_str()
                        .ok_or_else(|| invalid("authenticated login"))
                })
                .transpose()?;
            let parts = match owner {
                Some(org) if !login.is_some_and(|l| l.eq_ignore_ascii_case(org)) => {
                    vec!["orgs", org, "repos"]
                }
                _ => vec!["user", "repos"],
            };
            let mut b = json!({"name":name,"private":private});
            if c.kind == "forgejo" {
                b["auto_init"] = false.into();
            }
            let v = send(c, Method::POST, path(c, &parts)?, Some(b), true).await?;
            if owner.is_none() {
                target.repository = created_path(&v, "full_name", name)?;
            }
            v
        }
        "gitlab" => {
            let mut b = json!({"name":name,"path":name,"visibility":visibility});
            if let Some(ns) = owner {
                let namespace = get(c, path(c, &["namespaces", ns])?).await?;
                b["namespace_id"] = json!(number(&namespace["id"])?);
            }
            let v = send(c, Method::POST, path(c, &["projects"])?, Some(b), true).await?;
            if owner.is_none() {
                target.repository = created_path(&v, "path_with_namespace", name)?;
            }
            v
        }
        "bitbucket" => {
            send(
                c,
                Method::POST,
                rest::project(c)?,
                Some(json!({"scm":"git","is_private":private})),
                true,
            )
            .await?
        }
        "azure-devops" => azure::create_repository(c, private).await?,
        "tanstack-source" => {
            let owner = owner.ok_or_else(|| ForgeError::bad("Source repository needs an owner"))?;
            send(
                c,
                Method::POST,
                path(c, &["repos"])?,
                Some(json!({"name":name,"owner":owner,"visibility":visibility})),
                true,
            )
            .await?
        }
        "cursor-origin" => {
            if !private {
                return Err(ForgeError::unsupported(
                    "Origin repositories can only be published privately",
                ));
            }
            let owner = match owner {
                Some(s) => s.to_owned(),
                None => {
                    let v = get(c, path(c, &["namespaces"])?).await?;
                    v["namespaces"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|e| e["namespace"]["type"] == "user")
                        .and_then(|e| e["namespace"]["slug"].as_str())
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| invalid("Origin personal namespace"))?
                        .to_owned()
                }
            };
            target.repository = format!("{owner}/{name}");
            send(
                c,
                Method::POST,
                path(c, &["repos", &owner])?,
                Some(json!({"name":name})),
                true,
            )
            .await?;
            let mut attempt = 0;
            loop {
                match send(
                    c,
                    Method::PATCH,
                    hosted::resource(&target, None, None)?,
                    Some(json!({"visibility":"private"})),
                    true,
                )
                .await
                {
                    Ok(v)
                        if v["name"] == name
                            && (v["visibility"].is_null() || v["visibility"] == "private") =>
                    {
                        break v;
                    }
                    Err(e) if attempt < 2 && !matches!(e.status, 401 | 429) => {
                        attempt += 1;
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    _ => {
                        return Err(ForgeError::uncertain(
                            "Origin created the repository but did not confirm private visibility; the team may still see it",
                        ));
                    }
                }
            }
        }
        "gitcafe" => {
            let owner =
                owner.ok_or_else(|| ForgeError::bad("GitCafe repository needs an organization"))?;
            let mut v=send(c,Method::POST,path(c,&["orgs",owner,"repos"])?,Some(json!({"name":name,"visibility":visibility,"defaultBranch":"main","requestKey":request_id})),true).await?;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                if v["owner"] != owner || v["name"] != name {
                    return Err(ForgeError::uncertain("GitCafe admission identity changed"));
                }
                match v["state"].as_str() {
                    Some("complete") => break,
                    Some("blocked") => {
                        return Err(ForgeError::bad("GitCafe blocked repository admission"));
                    }
                    _ => {}
                }
                let id = v["repoId"].as_str().ok_or_else(|| {
                    ForgeError::uncertain("GitCafe did not confirm the admission ID")
                })?;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                v = tokio::time::timeout_at(
                    deadline,
                    get(c, path(c, &["orgs", owner, "admissions", id])?),
                )
                .await
                .map_err(|_| ForgeError::uncertain("GitCafe admission is still pending"))?
                .map_err(|_| ForgeError::uncertain("GitCafe admission status is unconfirmed"))?;
            }
            v
        }
        _ => return Err(ForgeError::unsupported("no repository creation driver")),
    };
    clone_urls(&target, &v).map_err(|_| {
        ForgeError::uncertain(
            "repository creation response did not confirm its identity and clone URLs",
        )
    })
}

fn created_path(raw: &Value, field: &str, expected_name: &str) -> Result<String, ForgeError> {
    raw[field]
        .as_str()
        .filter(|s| {
            s.rsplit_once('/').is_some_and(|(owner, n)| {
                !owner.is_empty() && n.eq_ignore_ascii_case(expected_name)
            })
        })
        .map(str::to_owned)
        .ok_or_else(|| ForgeError::uncertain("created repository name was not confirmed"))
}
