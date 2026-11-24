use super::issue::child;
use super::*;
use axum::{
    Json,
    extract::{Path, State},
};
use regex::Regex;
use std::sync::LazyLock;
use uuid::Uuid;

pub(in crate::workbench::forge) async fn breakdown(
    State(app): State<crate::workbench::Workbench>,
    Path((project, n)): Path<(Uuid, u64)>,
) -> Result<Json<Value>, ForgeError> {
    let mut c = Context::open(app, project).await?;
    if n == 0 {
        return Err(ForgeError::bad("pull number must be positive"));
    }
    if !matches!(c.kind.as_str(), "github" | "gitlab" | "tanstack-source") {
        return Ok(Json(Value::Null));
    }
    if let Some(token) = credentials::resolve(&c).await? {
        c.token = Some(token);
    }
    let rows = match c.kind.as_str() {
        "github" => github_files(&c, n).await?,
        "gitlab" => gitlab_files(&c, n).await?,
        _ => {
            let u = child(hosted::resource(&c, Some("pulls"), Some(n))?, &["files"]);
            let data = get(&c, u).await?;
            if data["truncated"] == true {
                None
            } else {
                Some(decode(&data["files"], true)?)
            }
        }
    };
    let Some(rows) = rows else {
        return Ok(Json(Value::Null));
    };
    let mut counts = [[0u64; 2]; 5];
    for row in rows {
        let bucket = &mut counts[classify(&row.path)];
        for (total, delta) in bucket.iter_mut().zip([row.additions, row.deletions]) {
            *total = total
                .checked_add(delta)
                .ok_or_else(|| invalid("line count overflow"))?;
        }
    }
    let mut response = serde_json::Map::new();
    for (label, [additions, deletions]) in ["code", "tests", "mocks", "generated", "docs"]
        .into_iter()
        .zip(counts)
    {
        response.insert(
            label.into(),
            json!({ "additions":additions, "deletions":deletions }),
        );
    }
    Ok(Json(Value::Object(response)))
}

struct FileStat {
    path: String,
    additions: u64,
    deletions: u64,
}
fn decode(raw: &Value, defaults: bool) -> Result<Vec<FileStat>, ForgeError> {
    if raw.is_null() {
        return Ok(Vec::new());
    }
    raw.as_array()
        .ok_or_else(|| invalid("file statistics list"))?
        .iter()
        .filter(|v| !v.is_null())
        .map(|v| {
            let lines = |s: &str| {
                if defaults && v[s].is_null() {
                    Ok(0)
                } else {
                    v[s].as_u64().ok_or_else(|| invalid("file change count"))
                }
            };
            Ok(FileStat {
                path: v["path"]
                    .as_str()
                    .ok_or_else(|| invalid("file path"))?
                    .into(),
                additions: lines("additions")?,
                deletions: lines("deletions")?,
            })
        })
        .collect()
}

async fn github_files(c: &Context, n: u64) -> Result<Option<Vec<FileStat>>, ForgeError> {
    let (owner, repo) = c
        .repository
        .split_once('/')
        .ok_or_else(|| invalid("repository"))?;
    const QUERY: &str = "query KarFileStats($owner:String!,$name:String!,$number:Int!,$after:String){repository(owner:$owner,name:$name){pullRequest(number:$number){files(first:100,after:$after){pageInfo{hasNextPage endCursor} nodes{path additions deletions}}}}}";
    let mut after = Value::Null;
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    for _ in 0..5 {
        let result = reviews::github::gql(
            c,
            QUERY,
            json!({"owner":owner,"name":repo,"number":n,"after":after}),
            false,
        )
        .await?;
        let files = &result["repository"]["pullRequest"]["files"];
        rows.extend(
            decode(&files["nodes"], false)?
                .into_iter()
                .filter(|r| !r.path.is_empty()),
        );
        if files["pageInfo"]["hasNextPage"] != true {
            return Ok(Some(rows));
        }
        let cursor = files["pageInfo"]["endCursor"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("file pagination cursor"))?;
        if !seen.insert(cursor.to_owned()) {
            return Err(invalid("repeated file pagination cursor"));
        }
        after = json!(cursor);
    }
    Ok(None)
}

async fn gitlab_files(c: &Context, n: u64) -> Result<Option<Vec<FileStat>>, ForgeError> {
    const QUERY: &str = "query KarFileStats($fullPath:ID!,$iid:String!){project(fullPath:$fullPath){mergeRequest(iid:$iid){diffStatsSummary{fileCount} diffStats{path additions deletions}}}}";
    let v = send(
        c,
        Method::POST,
        super::rest::gitlab_graphql(c),
        Some(json!({"query":QUERY,"variables":{"fullPath":c.repository,"iid":n.to_string()}})),
        false,
    )
    .await?;
    if v["errors"].as_array().is_some_and(|e| !e.is_empty()) {
        return Err(ForgeError::remote(
            502,
            "GitLab rejected the file statistics query",
        ));
    }
    let mr = &v["data"]["project"]["mergeRequest"];
    let rows = decode(&mr["diffStats"], false)?;
    Ok(mr["diffStatsSummary"]["fileCount"]
        .as_u64()
        .filter(|n| *n <= rows.len() as u64)
        .map(|_| rows))
}

fn classify(path: &str) -> usize {
    static GENERATED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(\.generated\.|\.gen\.|\.g\.dart$|\.pb\.go$|_pb2(_grpc)?\.py$|\.pb\.(cc|h)$|\.min\.(js|css)$|\.(js|css)\.map$)").unwrap()
    });
    static MOCK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(\.snap$|\.(mocks?|fixtures?|stubs?|fakes?)\.[^.]+$|^(mocks?|fixtures?|stubs?|fakes?)([-_.A-Z]|$))").unwrap()
    });
    static TEST: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i:\.(test|spec|e2e|eval|stories)\.[^.]+$|_(test|spec)\.[^.]+$|^test_.+\.py$|^conftest\.py$|^(vitest|jest)\.setup\.[^.]+$)|(Tests?|Spec)\.(java|kt|kts|scala|groovy|cs|php|swift|m)$").unwrap()
    });
    static DOC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\.(md|mdx|markdown|rst|adoc|txt)$").unwrap());
    static CONFIG_TXT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(requirements.*|constraints.*|CMakeLists|robots|llms)\.txt$").unwrap()
    });
    let mut parts = path.rsplit(['/', '\\']);
    let name = parts.next().unwrap_or_default();
    let dirs: Vec<_> = parts.map(str::to_ascii_lowercase).collect();
    let under = |names: &[&str]| dirs.iter().any(|d| names.contains(&d.as_str()));
    if matches!(
        name.to_ascii_lowercase().as_str(),
        "bun.lock"
            | "bun.lockb"
            | "cargo.lock"
            | "composer.lock"
            | "deno.lock"
            | "flake.lock"
            | "gemfile.lock"
            | "go.sum"
            | "gradle.lockfile"
            | "mix.lock"
            | "npm-shrinkwrap.json"
            | "package-lock.json"
            | "packages.lock.json"
            | "pipfile.lock"
            | "pnpm-lock.yaml"
            | "podfile.lock"
            | "poetry.lock"
            | "pubspec.lock"
            | "uv.lock"
            | "yarn.lock"
    ) || GENERATED.is_match(name)
        || under(&["__generated__", "_generated", "generated"])
    {
        3
    } else if MOCK.is_match(name)
        || under(&[
            "__fixtures__",
            "__mocks__",
            "__snapshots__",
            "fakes",
            "fixture",
            "fixtures",
            "mock",
            "mocks",
            "stubs",
            "testdata",
        ])
    {
        2
    } else if TEST.is_match(name)
        || under(&[
            "__tests__",
            "__test__",
            "cypress",
            "e2e",
            "evals",
            "spec",
            "specs",
            "test",
            "tests",
        ])
    {
        1
    } else if DOC.is_match(name) && !CONFIG_TXT.is_match(name) {
        4
    } else {
        0
    }
}
