//! Azure repository identity includes the organization and Boards project.
use super::*;
use std::collections::HashMap;

struct Location<'a> {
    org: &'a str,
    project: &'a str,
    repo: &'a str,
    legacy: bool,
}
impl<'a> Location<'a> {
    fn from(c: &'a Context) -> Result<Self, ForgeError> {
        let parts: Vec<_> = c.repository.split('/').collect();
        let legacy = c.host.strip_suffix(".visualstudio.com");
        let (org, project, repo) = match (legacy, parts.as_slice()) {
            (Some(org), [project, "_git", repo])
            | (Some(org), ["DefaultCollection", project, "_git", repo]) => (org, *project, *repo),
            (Some(org), ["_git", repo]) => (org, *repo, *repo),
            (None, ["v3", org, project, repo]) | (None, [org, project, "_git", repo]) => {
                (*org, *project, *repo)
            }
            _ => return Err(ForgeError::bad("unknown Azure DevOps repository path")),
        };
        Ok(Self {
            org,
            project,
            repo,
            legacy: legacy.is_some(),
        })
    }
    fn url(
        &self,
        c: &Context,
        in_project: bool,
        area: &str,
        extra: &[&str],
    ) -> Result<Url, ForgeError> {
        let mut segments = vec![];
        if !self.legacy {
            segments.push(self.org);
        }
        if in_project {
            segments.push(self.project);
        }
        segments.extend(["_apis", area]);
        segments.extend(extra);
        let mut u = path(c, &segments)?;
        u.query_pairs_mut().append_pair("api-version", "7.1");
        Ok(u)
    }
    fn git(&self, c: &Context, tail: &[&str]) -> Result<Url, ForgeError> {
        let mut s = vec!["repositories", self.repo];
        s.extend(tail);
        self.url(c, true, "git", &s)
    }
    fn check_repo(&self, r: &Value) -> Result<(), ForgeError> {
        if !r["name"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(self.repo))
            || !r["project"]["name"]
                .as_str()
                .is_some_and(|v| v.eq_ignore_ascii_case(self.project))
        {
            return Err(invalid("Azure repository or project identity"));
        }
        Ok(())
    }
    fn pull(&self, r: &Value) -> Result<Value, ForgeError> {
        let n = number(&r["pullRequestId"])?;
        self.check_repo(&r["repository"])?;
        let branch = |v: &Value| {
            v.as_str()
                .and_then(|s| s.strip_prefix("refs/heads/"))
                .map(str::to_owned)
                .ok_or_else(|| invalid("Azure branch name"))
        };
        let state = match r["status"].as_str() {
            Some("active") => "open",
            Some("completed") => "merged",
            Some("abandoned") => "closed",
            _ => return Err(invalid("Azure pull state")),
        };
        let fallback = format!(
            "https://dev.azure.com/{}/{}/_git/{}/pullrequest/{n}",
            self.org, self.project, self.repo
        );
        Ok(
            json!({"number":n,"title":r["title"],"body":r["description"],"state":state,
            "url":r.pointer("/_links/web/href").and_then(Value::as_str).unwrap_or(&fallback),"updated_at":r.get("closedDate").filter(|v|!v.is_null()).unwrap_or(&r["creationDate"]),
            "single_repository":r["forkSource"].is_null(),"head_ref":branch(&r["sourceRefName"])?,"base_ref":branch(&r["targetRefName"])?,"base_sha":r.pointer("/lastMergeTargetCommit/commitId"),"head_sha":r.pointer("/lastMergeSourceCommit/commitId")}),
        )
    }
    async fn confirmed_pull(&self, c: &Context, n: u64) -> Result<Value, ForgeError> {
        let id = n.to_string();
        let raw = get(c, self.git(c, &["pullrequests", &id])?).await?;
        let mut pull = verify(self.pull(&raw)?, n)?;
        // The PR's lastMergeSourceCommit is a trial merge and can lag a push.
        // Azure ties the actual source and comparison base to a numbered iteration.
        let iteration = if raw["supportsIterations"] == false {
            None
        } else {
            let v = get(c, self.git(c, &["pullrequests", &id, "iterations"])?).await?;
            v["value"]
                .as_array()
                .ok_or_else(|| invalid("Azure pull iterations"))?
                .iter()
                .filter(|it| it["id"].as_u64().is_some())
                .max_by_key(|it| it["id"].as_u64())
                .cloned()
        };
        pull["head_sha"] = iteration
            .as_ref()
            .and_then(|it| it.pointer("/sourceRefCommit/commitId"))
            .cloned()
            .unwrap_or_default();
        pull["base_sha"] = iteration
            .as_ref()
            .and_then(|it| it.pointer("/commonRefCommit/commitId"))
            .cloned()
            .unwrap_or_default();
        if let Some(updated) = iteration.as_ref().and_then(|it| it["updatedDate"].as_str())
            && let Ok(time) = updated.parse::<jiff::Timestamp>()
            && pull["updated_at"]
                .as_str()
                .and_then(|s| s.parse::<jiff::Timestamp>().ok())
                .is_none_or(|previous| previous < time)
        {
            pull["updated_at"] = json!(updated);
        }
        Ok(pull)
    }
    fn issue(&self, v: &Value) -> Result<Value, ForgeError> {
        let fields = &v["fields"];
        if !fields["System.TeamProject"]
            .as_str()
            .is_some_and(|p| p.eq_ignore_ascii_case(self.project))
        {
            return Err(ForgeError::remote(
                404,
                "work item is outside the Azure project",
            ));
        }
        let n = number(&v["id"])?;
        let body_field = issue_body_field(fields);
        Ok(
            json!({"number":n,"body_field":body_field,"state":fields["System.State"],"title":fields["System.Title"],"body":fields[body_field],
            "updated_at":fields["System.ChangedDate"],"url":v.pointer("/_links/html/href"),"work_item_type":fields["System.WorkItemType"],"revision":v["rev"]}),
        )
    }
    async fn detail(&self, c: &Context, n: u64) -> Result<Value, ForgeError> {
        verify(
            self.issue(&get(c, self.url(c, true, "wit", &["workitems", &n.to_string()])?).await?)?,
            n,
        )
    }
    async fn states(&self, c: &Context, ty: &str) -> Result<Value, ForgeError> {
        let data = get(
            c,
            self.url(c, true, "wit", &["workitemtypes", ty, "states"])?,
        )
        .await?;
        if !data["value"].is_array() {
            return Err(invalid("Azure work item state directory"));
        }
        Ok(data["value"].clone())
    }
    async fn issues(&self, c: &Context) -> Result<Value, ForgeError> {
        let mut result = vec![];
        let mut catalog = HashMap::new();
        let mut last = 0;
        loop {
            let mut url = self.url(c, true, "wit", &["wiql"])?;
            url.query_pairs_mut().append_pair("$top", "200");
            let query = format!(
                "SELECT [System.Id] FROM WorkItems WHERE [System.TeamProject] = @project AND [System.Id] > {last} ORDER BY [System.Id]"
            );
            let page = send(c, Method::POST, url, Some(json!({"query":query})), false).await?;
            let ids = page["workItems"]
                .as_array()
                .ok_or_else(|| invalid("Azure WIQL list"))?
                .iter()
                .map(|r| number(&r["id"]))
                .collect::<Result<Vec<_>, _>>()?;
            if ids.is_empty() {
                break;
            }
            if ids[0] <= last || ids.windows(2).any(|w| w[0] >= w[1]) {
                return Err(invalid("Azure WIQL page order"));
            }
            last = *ids.last().unwrap();
            let batch = send(
                c,
                Method::POST,
                self.url(c, true, "wit", &["workitemsbatch"])?,
                Some(json!({"ids":ids,"errorPolicy":"Fail"})),
                false,
            )
            .await?;
            let rows = batch["value"]
                .as_array()
                .ok_or_else(|| invalid("Azure work item batch"))?;
            let mut pending: std::collections::BTreeSet<_> = ids.into_iter().collect();
            for row in rows {
                let issue = self.issue(row)?;
                if !pending.remove(&number(&issue["number"])?) {
                    return Err(invalid("Azure WIQL batch membership"));
                }
                let ty = issue["work_item_type"]
                    .as_str()
                    .ok_or_else(|| invalid("Azure work item type"))?;
                if !catalog.contains_key(ty) {
                    catalog.insert(ty.to_owned(), self.states(c, ty).await?);
                }
                let state = catalog[ty]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|s| s["name"] == issue["state"])
                    .ok_or_else(|| invalid("unknown Azure work item state"))?;
                if !matches!(state["category"].as_str(), Some("Completed" | "Removed")) {
                    result.push(issue);
                }
            }
            if !pending.is_empty() {
                return Err(invalid("incomplete Azure WIQL batch"));
            }
        }
        Ok(json!(result))
    }
}
pub(super) async fn execute(c: &Context, op: &Operation) -> Result<Value, ForgeError> {
    let l = Location::from(c)?;
    match op {
        op @ (Operation::SubmitReview { .. }
        | Operation::ReplyToThread { .. }
        | Operation::SetThreadResolution { .. }
        | Operation::ReviewThreads { .. }
        | Operation::ThreadComments { .. }) => super::reviews::execute(c, op).await,
        Operation::SetIssueReaction { .. } => super::reactions::execute(c, op, true).await,
        op @ (Operation::UpdateIssue { .. }
        | Operation::CreateIssue { .. }
        | Operation::CommentIssue { .. }
        | Operation::IssueComments { .. }) => issue_actions::execute(c, op).await,
        Operation::Identity => {
            let r = get(c, l.git(c, &[])?).await?;
            l.check_repo(&r)?;
            let account = if c.token.is_some() {
                let mut parts = vec![];
                if !l.legacy {
                    parts.push(l.org);
                }
                parts.extend(["_apis", "connectionData"]);
                let identity = get(c, path(c, &parts)?).await?;
                let user = &identity["authenticatedUser"];
                user.get("providerDisplayName")
                    .or_else(|| user.get("displayName"))
                    .or_else(|| user.get("id"))
                    .cloned()
                    .ok_or_else(|| invalid("authenticated Azure account"))?
            } else {
                Value::Null
            };
            Ok(
                json!({"host":c.host,"repository":c.repository,"repository_id":r["id"],"project_id":r["project"]["id"],"account":account,"default_branch":r["defaultBranch"].as_str().and_then(|s|s.strip_prefix("refs/heads/")),"permissions":null}),
            )
        }
        Operation::Pull { number: n } => l.confirmed_pull(c, *n).await,
        Operation::Pulls => {
            let mut out = vec![];
            let mut seen = std::collections::HashSet::new();
            loop {
                let mut u = l.git(c, &["pullrequests"])?;
                u.query_pairs_mut()
                    .append_pair("searchCriteria.status", "active")
                    .append_pair("$top", "100")
                    .append_pair("$skip", &out.len().to_string());
                let p = get(c, u).await?;
                let rows = p["value"]
                    .as_array()
                    .ok_or_else(|| invalid("Azure pull listing"))?;
                if rows.is_empty() {
                    break;
                }
                for r in rows {
                    let v = l.pull(r)?;
                    if !seen.insert(number(&v["number"])?) {
                        return Err(invalid("repeated Azure pull page"));
                    }
                    out.push(v);
                }
            }
            Ok(json!(out))
        }
        Operation::CreatePull {
            head,
            head_repository: _,
            base,
            title,
            body,
        } => {
            let body = json!({"sourceRefName":format!("refs/heads/{head}"),"targetRefName":format!("refs/heads/{base}"),"title":title,"description":body});
            let r = send(
                c,
                Method::POST,
                l.git(c, &["pullrequests"])?,
                Some(body),
                true,
            )
            .await?;
            l.pull(&r).map_err(|_| {
                ForgeError::uncertain("Azure did not identify the created pull request")
            })
        }
        Operation::Issues => l.issues(c).await,
        Operation::Issue { number } => l.detail(c, *number).await,
        Operation::SetIssueState { number: n, state } => {
            let existing = l.detail(c, *n).await?;
            let ty = existing["work_item_type"]
                .as_str()
                .ok_or_else(|| invalid("Azure work item type"))?;
            if !l
                .states(c, ty)
                .await?
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == *state)
            {
                return Err(ForgeError::bad(
                    "unknown state for the Azure work item type",
                ));
            }
            let rev = number(&existing["revision"])?;
            let patch = json!([{"op":"test","path":"/rev","value":rev},{"op":"add","path":"/fields/System.State","value":state}]);
            let reply = send(
                c,
                Method::PATCH,
                l.url(c, true, "wit", &["workitems", &n.to_string()])?,
                Some(patch),
                true,
            )
            .await?;
            let verified = l
                .issue(&reply)
                .and_then(|v| verify(v, *n))
                .map_err(|_| ForgeError::uncertain("updated work item identity is unconfirmed"))?;
            if verified["state"] != *state {
                return Err(ForgeError::uncertain(
                    "Azure did not confirm the requested state",
                ));
            }
            Ok(verified)
        }
    }
}

mod issue_actions;

pub(in crate::workbench::forge) async fn repository(c: &Context) -> Result<Value, ForgeError> {
    let l = Location::from(c)?;
    let r = get(c, l.git(c, &[])?).await?;
    l.check_repo(&r)?;
    Ok(r)
}

pub(super) async fn candidates(c: &Context) -> Result<Value, ForgeError> {
    let l = Location::from(c)?;
    let tags = get(c, l.url(c, true, "wit", &["tags"])?).await?;
    let labels = tags["value"]
        .as_array()
        .ok_or_else(|| invalid("Azure project tags"))?
        .iter()
        .filter_map(|t| t["name"].as_str())
        .map(|s| json!({"name":s,"color":null,"description":null}))
        .collect::<Vec<_>>();
    let mut url = l.url(c, false, "projects", &[l.project, "teams"])?;
    url.query_pairs_mut().append_pair("$top", "10");
    let teams = get(c, url).await?;
    let mut users = std::collections::BTreeMap::new();
    for team in teams["value"]
        .as_array()
        .ok_or_else(|| invalid("Azure project teams"))?
        .iter()
        .take(10)
    {
        let id = team["id"]
            .as_str()
            .ok_or_else(|| invalid("Azure team ID"))?;
        let mut u = l.url(c, false, "projects", &[l.project, "teams", id, "members"])?;
        u.query_pairs_mut().append_pair("$top", "100");
        let members = get(c, u).await?;
        for m in members["value"]
            .as_array()
            .ok_or_else(|| invalid("Azure team members"))?
        {
            let v = &m["identity"];
            if v["isContainer"] == true {
                continue;
            }
            if let Some(login) = v["uniqueName"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                users.entry(login.to_lowercase()).or_insert_with(||json!({"id":login,"login":login,"name":v["displayName"],"avatarUrl":v["imageUrl"]}));
            }
        }
    }
    Ok(
        json!({"labels":labels,"assignees":users.into_values().collect::<Vec<_>>(),"truncated":true}),
    )
}

pub(super) async fn create_repository(c: &Context, private: bool) -> Result<Value, ForgeError> {
    let l = Location::from(c)?;
    // Azure sets visibility on the project. Check it before publishing a repository into it.
    let project = get(c, l.url(c, false, "projects", &[l.project])?).await?;
    let expected = if private { "private" } else { "public" };
    if !project["visibility"]
        .as_str()
        .is_some_and(|v| v.eq_ignore_ascii_case(expected))
    {
        return Err(ForgeError::bad(format!(
            "Azure project does not confirm {expected} visibility"
        )));
    }
    let id = project["id"]
        .as_str()
        .ok_or_else(|| invalid("Azure project ID"))?;
    let raw = send(
        c,
        Method::POST,
        l.url(c, true, "git", &["repositories"])?,
        Some(json!({"name":l.repo,"project":{"id":id}})),
        true,
    )
    .await?;
    l.check_repo(&raw).map_err(|_| {
        ForgeError::uncertain("Azure did not confirm the requested repository identity")
    })?;
    Ok(raw)
}

pub(in crate::workbench::forge::providers) async fn issue_fields(
    c: &Context,
    n: u64,
) -> Result<Value, ForgeError> {
    let l = Location::from(c)?;
    let raw = get(c, l.url(c, true, "wit", &["workitems", &n.to_string()])?).await?;
    let mut item = verify(l.issue(&raw)?, n)?;
    let fields = &raw["fields"];
    item["labels"] = json!(
        fields["System.Tags"]
            .as_str()
            .into_iter()
            .flat_map(|s| s.split(';'))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    );
    item["assignees"] = json!(
        fields
            .get("System.AssignedTo")
            .filter(|v| !v.is_null() && v.as_str() != Some(""))
            .into_iter()
            .collect::<Vec<_>>()
    );
    Ok(item)
}

pub(super) async fn reaction(
    c: &Context,
    n: u64,
    subject: Option<&str>,
    content: super::reactions::Content,
    want: bool,
    write: bool,
) -> Result<(), ForgeError> {
    let comment = subject
        .ok_or_else(|| ForgeError::unsupported("Azure Boards reactions require a comment"))?;
    let id = comment
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0 && comment.len() <= 10 && id.to_string() == comment)
        .ok_or_else(|| ForgeError::bad("invalid Azure comment ID"))?;
    let kind = content
        .azure()
        .ok_or_else(|| ForgeError::unsupported("Azure Boards has no such reaction"))?;
    let location = Location::from(c)?;
    location.detail(c, n).await?;
    let number = n.to_string();
    let parts = ["workitems", &number, "comments", comment];
    let mut url = location.url(c, true, "wit", &parts)?;
    url.query_pairs_mut()
        .clear()
        .append_pair("api-version", "7.1-preview.4")
        .append_pair("$expand", "reactions");
    let row = get(c, url.clone()).await?;
    if row["id"].as_u64() != Some(id)
        || row.get("workItemId").is_some_and(|v| v.as_u64() != Some(n))
        || row["isDeleted"] == true
    {
        return Err(ForgeError::remote(
            404,
            "comment does not belong to the selected work item",
        ));
    }
    let mut engaged = false;
    if !row["reactions"].is_null() {
        let entries = row["reactions"]
            .as_array()
            .ok_or_else(|| invalid("Azure comment reactions"))?;
        if let Some(entry) = entries.iter().find(|r| {
            r["type"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(kind))
        }) {
            engaged = entry["isCurrentUserEngaged"]
                .as_bool()
                .ok_or_else(|| invalid("Azure viewer reaction"))?;
        }
    }
    if super::reactions::satisfied(engaged, want, write)? {
        return Ok(());
    }
    url.set_query(None);
    let mut url = super::issue::child(url, &["reactions", kind]);
    url.query_pairs_mut()
        .append_pair("api-version", "7.1-preview.1");
    send(
        c,
        if want { Method::PUT } else { Method::DELETE },
        url,
        None,
        true,
    )
    .await?;
    Ok(())
}

pub(super) async fn viewer_id(c: &Context) -> Result<String, ForgeError> {
    let loc = Location::from(c)?;
    let identity = get(c, loc.url(c, false, "connectionData", &[])?).await?;
    identity["authenticatedUser"]["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("authenticated Azure user ID"))
}

pub(super) fn issue_scope(c: &Context) -> Result<String, ForgeError> {
    let l = Location::from(c)?;
    Ok(if l.legacy {
        l.project.into()
    } else {
        format!("{}/{}", l.org, l.project)
    })
}
pub(super) fn issue_body_field(fields: &Value) -> &'static str {
    let present = |key: &str| fields[key].as_str().is_some_and(|s| !s.trim().is_empty());
    if !present("System.Description")
        && (fields["System.WorkItemType"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("Bug"))
            || present("Microsoft.VSTS.TCM.ReproSteps"))
    {
        "Microsoft.VSTS.TCM.ReproSteps"
    } else {
        "System.Description"
    }
}
pub(super) fn git_endpoint(c: &Context, tail: &[&str]) -> Result<Url, ForgeError> {
    Location::from(c)?.git(c, tail)
}
pub(super) fn organization_pull(c: &Context, n: u64) -> Result<Url, ForgeError> {
    Location::from(c)?.url(c, false, "git", &["pullrequests", &n.to_string()])
}
pub(super) fn wit_endpoint(c: &Context, parts: &[&str]) -> Result<Url, ForgeError> {
    Location::from(c)?.url(c, true, "wit", parts)
}

pub(super) fn repository_for_issue_scope(c: &Context, scope: &str) -> Result<String, ForgeError> {
    let l = Location::from(c)?;
    if l.legacy && !scope.contains('/') {
        return Ok(format!("{scope}/_git/{}", l.repo));
    }
    if !l.legacy
        && let Some((org, p)) = scope.split_once('/')
        && !p.contains('/')
    {
        return Ok(format!("{org}/{p}/_git/{}", l.repo));
    }
    Err(ForgeError::bad("invalid Azure work item project scope"))
}
