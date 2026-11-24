use super::super::issue::child;
use super::*;
pub(super) async fn apply(
    c: &Context,
    n: u64,
    comment: Option<&str>,
    content: Content,
    want: bool,
    write: bool,
) -> Result<(), ForgeError> {
    let gitlab = c.kind == "gitlab";
    let repo = super::super::rest::project(c)?;
    let target = child(repo.clone(), &["issues", &n.to_string()]);
    let issue = get(c, target.clone()).await?;
    super::super::issue::verify_issue(c, &issue, n)?;
    let url = if let Some(id) = comment {
        positive(id)?;
        if gitlab {
            child(target, &["notes", id, "award_emoji"])
        } else {
            let comment = child(repo, &["issues", "comments", id]);
            verify_forgejo_comment(c, &target, id).await?;
            child(comment, &["reactions"])
        }
    } else {
        child(target, &[if gitlab { "award_emoji" } else { "reactions" }])
    };
    let viewer = get(c, path(c, &["user"])?).await?;
    let key = if gitlab { "username" } else { "login" };
    let viewer = viewer[key]
        .as_str()
        .ok_or_else(|| invalid("authenticated forge viewer"))?;
    let name = if gitlab {
        content.gitlab()
    } else {
        content.standard()
    };
    let mut own = None;
    let mut complete = false;
    for page in 1..=200 {
        let mut u = url.clone();
        if gitlab {
            u.query_pairs_mut()
                .append_pair("page", &page.to_string())
                .append_pair("per_page", "50");
        }
        let v = get(c, u).await?;
        let rows = v.as_array().ok_or_else(|| invalid("issue reaction list"))?;
        if let Some(r) = rows.iter().find(|r| {
            r[if gitlab { "name" } else { "content" }] == name
                && r["user"][key]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(viewer))
        }) {
            own = Some(r["id"].clone());
            complete = true;
            break;
        }
        if !gitlab || rows.len() < 50 {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err(ForgeError::remote(
            502,
            "reaction directory exceeded the page limit",
        ));
    }
    if satisfied(own.is_some(), want, write)? {
        return Ok(());
    }
    if gitlab {
        if want {
            let mut u = url;
            u.query_pairs_mut().append_pair("name", name);
            send(c, Method::POST, u, None, true).await?;
        } else {
            let id = number(&own.unwrap())?.to_string();
            send(c, Method::DELETE, child(url, &[&id]), None, true).await?;
        }
    } else {
        send(
            c,
            if want { Method::POST } else { Method::DELETE },
            url,
            Some(json!({"content":name})),
            true,
        )
        .await?;
    }
    Ok(())
}

async fn verify_forgejo_comment(c: &Context, target: &Url, id: &str) -> Result<(), ForgeError> {
    let n = positive(id)?;
    let result = get(c, child(target.clone(), &["comments"])).await?;
    if result
        .as_array()
        .ok_or_else(|| invalid("Forgejo issue comments"))?
        .iter()
        .any(|r| r["id"].as_u64() == Some(n))
    {
        Ok(())
    } else {
        Err(ForgeError::remote(
            404,
            "comment does not belong to the selected issue",
        ))
    }
}
