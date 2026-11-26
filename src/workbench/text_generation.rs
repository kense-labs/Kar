//! Lightweight pure-Rust AI text generation service with direct HTTP LLM drivers
//! (`openai_compatible` and `anthropic`) and quota-aware coding-agent fallback.
//!
//! Powers 6 workbench capabilities:
//! 1. `generate_commit_message` (honoring `source_control_writing_style`)
//! 2. `generate_pr_content` (honoring repo PR templates & writing style)
//! 3. `generate_branch_name` (semantic git-ref-safe branch naming)
//! 4. `generate_thread_title` (auto-titling chats with linked Issue/PR context)
//! 5. `generate_review_guide` (read-only checkout tour on Codex or Claude)
//! 6. `generate_transcription_post_processing` (filler stripping, custom vocabulary, LLM polish)

use super::{
    Catalog, Workbench, http::ApiError, mcp::collaboration_tools::environment,
    secrets::SecretService,
};
use crate::{Error, Outcome, Result, SessionOptions};
use axum::{Json, Router, extract::State, routing::post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, process::Command, time::Duration};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WritingStyle {
    pub mode: String,
    pub follow_change_request_templates: bool,
    pub custom_instructions: String,
}

impl Default for WritingStyle {
    fn default() -> Self {
        Self {
            mode: "repo_conventions".into(),
            follow_change_request_templates: true,
            custom_instructions: String::new(),
        }
    }
}

pub(in crate::workbench) fn writing_style(db: &Catalog) -> WritingStyle {
    let prefs = environment::prefs(db);
    let raw = &prefs["source_control_writing_style"];
    WritingStyle {
        mode: raw["mode"]
            .as_str()
            .unwrap_or("repo_conventions")
            .to_owned(),
        follow_change_request_templates: raw["follow_change_request_templates"]
            .as_bool()
            .unwrap_or(true),
        custom_instructions: raw["custom_instructions"].as_str().unwrap_or("").to_owned(),
    }
}

#[derive(Clone, Debug)]
enum HttpDriver {
    OpenAiCompatible {
        base_url: String,
        api_key: String,
        model: String,
    },
    Anthropic {
        base_url: String,
        api_key: String,
        model: String,
    },
}

async fn resolve_secret_or_env(
    app: Option<&Workbench>,
    secret_name: &str,
    env_var: &str,
) -> Option<String> {
    if let Ok(val) = std::env::var(env_var)
        && !val.trim().is_empty()
    {
        return Some(val);
    }
    if let Some(app) = app
        && let Ok(svc) = SecretService::for_workbench(app)
        && let Ok(val) = svc.resolve(secret_name).await
    {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    None
}

async fn discover_http_drivers(app: Option<&Workbench>) -> Vec<HttpDriver> {
    let mut drivers = Vec::new();
    if let Some(key) = resolve_secret_or_env(
        app,
        "text_generation_api_key",
        "KAR_TEXT_GENERATION_API_KEY",
    )
    .await
    {
        let provider = std::env::var("KAR_TEXT_GENERATION_PROVIDER")
            .unwrap_or_else(|_| "openai_compatible".into());
        if provider.eq_ignore_ascii_case("anthropic") {
            let base_url = std::env::var("KAR_TEXT_GENERATION_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".into());
            let model = std::env::var("KAR_TEXT_GENERATION_MODEL")
                .unwrap_or_else(|_| "claude-3-5-haiku-latest".into());
            drivers.push(HttpDriver::Anthropic {
                base_url,
                api_key: key,
                model,
            });
        } else {
            let base_url = std::env::var("KAR_TEXT_GENERATION_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".into());
            let model =
                std::env::var("KAR_TEXT_GENERATION_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
            drivers.push(HttpDriver::OpenAiCompatible {
                base_url,
                api_key: key,
                model,
            });
        }
    }
    if let Some(key) = resolve_secret_or_env(app, "openai_api_key", "OPENAI_API_KEY").await {
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let model = std::env::var("KAR_OPENAI_TEXT_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
        drivers.push(HttpDriver::OpenAiCompatible {
            base_url,
            api_key: key,
            model,
        });
    }
    if let Some(key) = resolve_secret_or_env(app, "anthropic_api_key", "ANTHROPIC_API_KEY").await {
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".into());
        let model = std::env::var("KAR_ANTHROPIC_TEXT_MODEL")
            .unwrap_or_else(|_| "claude-3-5-haiku-latest".into());
        drivers.push(HttpDriver::Anthropic {
            base_url,
            api_key: key,
            model,
        });
    }
    drivers
}

async fn call_http_driver(driver: &HttpDriver, system: &str, prompt: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    match driver {
        HttpDriver::OpenAiCompatible {
            base_url,
            api_key,
            model,
        } => {
            let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
            let resp = client
                .post(&url)
                .bearer_auth(api_key)
                .json(&json!({
                    "model": model,
                    "temperature": 0.2,
                    "max_tokens": 1024,
                    "messages": [
                        {"role": "system", "content": system},
                        {"role": "user", "content": prompt}
                    ]
                }))
                .send()
                .await?
                .error_for_status()?;
            let body: Value = resp.json().await?;
            let text = body["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .trim();
            if text.is_empty() {
                return Err(Error::Refused(
                    "OpenAI-compatible text generator returned empty content".into(),
                ));
            }
            Ok(text.to_owned())
        }
        HttpDriver::Anthropic {
            base_url,
            api_key,
            model,
        } => {
            let trimmed = base_url.trim_end_matches('/');
            let url = if trimmed.ends_with("/v1") {
                format!("{trimmed}/messages")
            } else {
                format!("{trimmed}/v1/messages")
            };
            let resp = client
                .post(&url)
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&json!({
                    "model": model,
                    "max_tokens": 1024,
                    "system": system,
                    "messages": [
                        {"role": "user", "content": prompt}
                    ]
                }))
                .send()
                .await?
                .error_for_status()?;
            let body: Value = resp.json().await?;
            let text = body["content"]
                .as_array()
                .and_then(|blocks| {
                    blocks
                        .iter()
                        .find_map(|b| (b["type"] == "text").then(|| b["text"].as_str()).flatten())
                })
                .unwrap_or("")
                .trim();
            if text.is_empty() {
                return Err(Error::Refused(
                    "Anthropic text generator returned empty content".into(),
                ));
            }
            Ok(text.to_owned())
        }
    }
}

async fn call_agent_driver(
    runtime_id: &str,
    model: Option<&str>,
    system: &str,
    prompt: &str,
) -> Result<String> {
    let rt = crate::runtime(runtime_id)?;
    let cwd = tempfile::tempdir()?;
    let mut opts = SessionOptions::new(cwd.path());
    opts.model = model.map(str::to_owned);
    opts.remove_env("KAR_WORKBENCH_TOKEN")?;
    let combined = if system.trim().is_empty() {
        prompt.to_owned()
    } else {
        format!("{system}\n\n{prompt}")
    };
    let session = rt.open(opts).await?;
    let result = async {
        let report = session.prompt(combined).await?.report().await?;
        let text = report.text.trim();
        if report.outcome != Outcome::Completed || text.is_empty() || text.len() > 24_000 {
            return Err(Error::Refused(format!(
                "{runtime_id} returned no usable generated text"
            )));
        }
        Ok(text.to_owned())
    }
    .await;
    let _ = session.close().await;
    result
}

/// Generate bounded text with automatic provider fallback:
/// 1) Explicit `requested_runtime` if requested by caller,
/// 2) Lightweight HTTP LLM drivers (`openai_compatible` / `anthropic`),
/// 3) Installed coding-agent runtimes (`codex`, `claude`, `antigravity`, `qwen`, `opencode`, `grok`).
pub async fn generate_text(
    app: Option<&Workbench>,
    system: &str,
    prompt: &str,
    requested_runtime: Option<&str>,
    requested_model: Option<&str>,
) -> Result<String> {
    let mut last_err = None;
    if let Some(rt) = requested_runtime.filter(|s| !s.trim().is_empty()) {
        match call_agent_driver(rt, requested_model, system, prompt).await {
            Ok(text) => return Ok(text),
            Err(e) => {
                // If caller explicitly pinned a writer_runtime and no HTTP fallback exists, keep trying fallbacks
                // unless requested_model was also pinned to that exact runtime.
                if requested_model.is_some() {
                    return Err(e);
                }
                last_err = Some(e);
            }
        }
    }
    for http in discover_http_drivers(app).await {
        match call_http_driver(&http, system, prompt).await {
            Ok(text) => return Ok(text),
            Err(e) => last_err = Some(e),
        }
    }
    for candidate in ["codex", "claude", "antigravity", "qwen", "opencode", "grok"] {
        if requested_runtime == Some(candidate) {
            continue;
        }
        let Ok(rt) = crate::runtime(candidate) else {
            continue;
        };
        if !matches!(
            rt.installation().await,
            crate::Installation::Available { .. }
        ) {
            continue;
        }
        match call_agent_driver(candidate, None, system, prompt).await {
            Ok(text) => return Ok(text),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        Error::Unsupported("no configured HTTP text generator or installed coding agent".into())
    }))
}

pub fn recent_commit_subjects(repo_root: &Path, count: usize) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(["log", "-n", &count.to_string(), "--pretty=%s"])
        .env_remove("KAR_WORKBENCH_TOKEN")
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

pub fn read_pr_template(repo_root: &Path) -> Option<String> {
    for rel in [
        ".github/PULL_REQUEST_TEMPLATE.md",
        ".github/pull_request_template.md",
        "docs/pull_request_template.md",
        "PULL_REQUEST_TEMPLATE.md",
        "pull_request_template.md",
    ] {
        let candidate = repo_root.join(rel);
        if let Ok(meta) = std::fs::symlink_metadata(&candidate)
            && meta.is_file()
            && meta.len() <= 64 * 1024
            && let Ok(content) = std::fs::read_to_string(&candidate)
            && !content.trim().is_empty()
        {
            return Some(content);
        }
    }
    None
}

pub fn build_commit_prompt(
    style: &WritingStyle,
    recent_subjects: &[String],
    diff_summary: &str,
) -> (String, String) {
    let mut system = String::from(
        "Write a Git commit message for the provided change. Return ONLY the raw commit message (concise subject line <= 72 chars, optionally followed by a blank line and brief body). Do not wrap in Markdown code fences.",
    );
    match style.mode.as_str() {
        "conventional_commits" => {
            system.push_str(
                " Use Conventional Commits format (`feat:`, `fix:`, `refactor:`, `chore:`, `docs:`, `test:`).",
            );
        }
        "concise" => {
            system.push_str(" Keep it to a single crisp imperative subject line with no body.");
        }
        _ => {
            if !recent_subjects.is_empty() {
                system.push_str(" Match the repository's existing commit message conventions.");
            }
        }
    }
    if !style.custom_instructions.trim().is_empty() {
        system.push_str("\nCustom repository instructions: ");
        system.push_str(style.custom_instructions.trim());
    }
    let mut prompt = String::new();
    if style.mode == "repo_conventions" && !recent_subjects.is_empty() {
        prompt.push_str("Recent commit subjects in this repository:\n");
        for s in recent_subjects.iter().take(5) {
            prompt.push_str("- ");
            prompt.push_str(s);
            prompt.push('\n');
        }
        prompt.push('\n');
    }
    prompt.push_str("Staged / working tree changes:\n");
    prompt.push_str(diff_summary);
    (system, prompt)
}

pub async fn generate_commit_message(
    app: &Workbench,
    repo_root: &Path,
    diff_summary: &str,
    writer_runtime: Option<&str>,
    writer_model: Option<&str>,
) -> Result<String> {
    let style = writing_style(&app.0.catalog.lock().unwrap());
    let recent = if style.mode == "repo_conventions" {
        recent_commit_subjects(repo_root, 5)
    } else {
        Vec::new()
    };
    let (system, prompt) = build_commit_prompt(&style, &recent, diff_summary);
    let raw = generate_text(Some(app), &system, &prompt, writer_runtime, writer_model).await?;
    Ok(strip_code_fence(&raw).to_owned())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrContent {
    pub title: String,
    pub body: String,
}

pub async fn generate_pr_content(
    app: &Workbench,
    repo_root: &Path,
    base_branch: Option<&str>,
    commit_message: &str,
    diff_summary: &str,
    writer_runtime: Option<&str>,
    writer_model: Option<&str>,
) -> PrContent {
    let fallback_title = commit_message
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Update changes")
        .to_owned();
    let fallback_body = commit_message
        .lines()
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();

    let style = writing_style(&app.0.catalog.lock().unwrap());
    let template = if style.follow_change_request_templates {
        read_pr_template(repo_root)
    } else {
        None
    };

    let mut system = String::from(
        "Generate a pull request title and Markdown description. Return ONLY valid JSON of the form {\"title\": \"...\", \"body\": \"...\"}.",
    );
    if !style.custom_instructions.trim().is_empty() {
        system.push_str("\nCustom instructions: ");
        system.push_str(style.custom_instructions.trim());
    }
    let mut prompt = format!(
        "Base branch: {}\nCommit message:\n{}\n\nDiff summary:\n{}",
        base_branch.unwrap_or("main"),
        commit_message,
        diff_summary
    );
    if let Some(tpl) = &template {
        prompt.push_str("\n\nFollow this repository pull request template for `body`:\n");
        prompt.push_str(tpl);
    }

    if let Ok(raw) = generate_text(Some(app), &system, &prompt, writer_runtime, writer_model).await
    {
        let cleaned = strip_code_fence(&raw);
        if let Ok(parsed) = serde_json::from_str::<PrContent>(cleaned)
            && !parsed.title.trim().is_empty()
        {
            return PrContent {
                title: parsed.title.trim().to_owned(),
                body: parsed.body.trim().to_owned(),
            };
        }
    }

    PrContent {
        title: fallback_title,
        body: fallback_body,
    }
}

pub fn sanitize_branch_slug(input: &str, fallback_id: Uuid) -> String {
    let mut slug = String::new();
    let mut prev_sep = true;
    for ch in input.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_sep = false;
        } else if (ch == '/' && !slug.contains('/') && !slug.is_empty()) && !prev_sep {
            slug.push('/');
            prev_sep = true;
        } else if !prev_sep {
            slug.push('-');
            prev_sep = true;
        }
        if slug.len() >= 56 {
            break;
        }
    }
    let trimmed = slug.trim_matches(['-', '/']);
    if trimmed.is_empty() {
        format!("kar/change-{}", &fallback_id.simple().to_string()[..8])
    } else if trimmed.contains('/') {
        trimmed.to_owned()
    } else {
        format!("kar/{trimmed}")
    }
}

pub async fn generate_branch_name(
    app: Option<&Workbench>,
    context: &str,
    writer_runtime: Option<&str>,
    writer_model: Option<&str>,
    fallback_id: Uuid,
) -> String {
    let system = "Generate a short semantic Git branch name (such as `feat/wal-store` or `fix/acp-cwd-decode`). Return ONLY the raw branch name in lowercase ASCII with hyphens.";
    if let Ok(raw) = generate_text(app, system, context, writer_runtime, writer_model).await {
        let first_line = strip_code_fence(&raw).lines().next().unwrap_or("").trim();
        if !first_line.is_empty() {
            return sanitize_branch_slug(first_line, fallback_id);
        }
    }
    sanitize_branch_slug(context, fallback_id)
}

pub fn deterministic_title_fallback(first_message: &str, linked_context: Option<&str>) -> String {
    let base = first_message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("New conversation");
    let combined = match linked_context.map(str::trim).filter(|s| !s.is_empty()) {
        Some(ctx) if !base.contains(ctx) => format!("{ctx}: {base}"),
        _ => base.to_owned(),
    };
    let mut out: String = combined.chars().take(60).collect();
    if combined.chars().count() > 60 {
        out.push('…');
    }
    out
}

pub async fn generate_thread_title(
    app: &Workbench,
    first_message: &str,
    linked_context: Option<&str>,
) -> String {
    let system = "Write a concise, descriptive chat title (3 to 8 words, <= 60 characters, matching the user's language). Return ONLY the plain title without quotes or trailing period.";
    let prompt = match linked_context.filter(|s| !s.trim().is_empty()) {
        Some(ctx) => {
            format!("Linked Issue/PR context: {ctx}\n\nFirst user message:\n{first_message}")
        }
        None => first_message.to_owned(),
    };
    if let Ok(raw) = generate_text(Some(app), system, &prompt, None, None).await {
        let line = strip_code_fence(&raw)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches(['"', '\'', '“', '”']);
        if !line.is_empty() {
            let mut capped: String = line.chars().take(64).collect();
            if line.chars().count() > 64 {
                capped.push('…');
            }
            return capped;
        }
    }
    deterministic_title_fallback(first_message, linked_context)
}

/// Fast, non-subprocess thread title generator for automatic first-turn naming and MCP regeneration:
/// uses configured HTTP LLM drivers (`openai_compatible` / `anthropic`) and falls back immediately
/// to [`deterministic_title_fallback`] without spawning extra coding-agent subprocesses.
pub async fn generate_thread_title_fast(
    app: &Workbench,
    first_message: &str,
    linked_context: Option<&str>,
) -> String {
    let system = "Write a concise, descriptive chat title (3 to 8 words, <= 60 characters, matching the user's language). Return ONLY the plain title without quotes or trailing period.";
    let prompt = match linked_context.filter(|s| !s.trim().is_empty()) {
        Some(ctx) => {
            format!("Linked Issue/PR context: {ctx}\n\nFirst user message:\n{first_message}")
        }
        None => first_message.to_owned(),
    };
    for http in discover_http_drivers(Some(app)).await {
        if let Ok(raw) = call_http_driver(&http, system, &prompt).await {
            let line = strip_code_fence(&raw)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches(['"', '\'', '“', '”']);
            if !line.is_empty() {
                let mut capped: String = line.chars().take(64).collect();
                if line.chars().count() > 64 {
                    capped.push('…');
                }
                return capped;
            }
        }
    }
    deterministic_title_fallback(first_message, linked_context)
}

pub fn strip_speech_fillers_and_apply_vocab(
    text: &str,
    custom_words: &[String],
    strip_fillers: bool,
) -> String {
    let mut cleaned = text.to_owned();
    if strip_fillers {
        for zh in ["呃，", "呃,", "额，", "额,", "那个，", "就是说，"] {
            cleaned = cleaned.replace(zh, "");
        }
        let fillers = ["um", "uh", "erm", "hmm", "uhh", "umm"];
        let mut words = Vec::new();
        for token in cleaned.split_whitespace() {
            let bare = token
                .trim_matches(|c: char| c.is_ascii_punctuation())
                .to_ascii_lowercase();
            if fillers.contains(&bare.as_str()) {
                continue;
            }
            words.push(token.to_owned());
        }
        if !words.is_empty() || text.trim().is_empty() {
            cleaned = words.join(" ");
        }
    }
    for vocab in custom_words {
        let v = vocab.trim();
        if v.is_empty() {
            continue;
        }
        let lower_v = v.to_lowercase();
        // Case-insensitive whole-token canonicalization for technical terms (e.g., "antigravity" -> "Antigravity", "gix" -> "gix").
        let mut rebuilt = Vec::new();
        for token in cleaned.split(' ') {
            let start = token.find(|c: char| !c.is_ascii_punctuation()).unwrap_or(0);
            let end = token
                .rfind(|c: char| !c.is_ascii_punctuation())
                .map(|i| i + token[i..].chars().next().unwrap().len_utf8())
                .unwrap_or(token.len());
            if start <= end && token[start..end].to_lowercase() == lower_v {
                rebuilt.push(format!("{}{}{}", &token[..start], v, &token[end..]));
            } else {
                rebuilt.push(token.to_owned());
            }
        }
        cleaned = rebuilt.join(" ");
    }
    cleaned.trim().to_owned()
}

pub async fn post_process_transcription(
    app: Option<&Workbench>,
    raw_text: &str,
    custom_words: &[String],
    strip_fillers: bool,
    llm_polish: bool,
) -> String {
    let local = strip_speech_fillers_and_apply_vocab(raw_text, custom_words, strip_fillers);
    if !llm_polish || local.is_empty() {
        return local;
    }
    let mut system = String::from(
        "Clean up this dictated speech transcript: fix punctuation and capitalization, remove spoken hesitations, and preserve every technical detail and the speaker's original language. Return ONLY the cleaned transcript.",
    );
    if !custom_words.is_empty() {
        system.push_str("\nPreferred domain vocabulary: ");
        system.push_str(&custom_words.join(", "));
    }
    match generate_text(app, &system, &local, None, None).await {
        Ok(out) if !out.trim().is_empty() => strip_code_fence(&out).to_owned(),
        _ => local,
    }
}

fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let body = rest.find('\n').map(|idx| &rest[idx + 1..]).unwrap_or(rest);
        if let Some(inner) = body.trim_end().strip_suffix("```") {
            return inner.trim();
        }
    }
    trimmed
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
struct CommitReq {
    project_id: Option<Uuid>,
    diff: String,
    runtime: Option<String>,
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
struct PrReq {
    project_id: Option<Uuid>,
    base: Option<String>,
    commit_message: String,
    diff: String,
    runtime: Option<String>,
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
struct BranchReq {
    context: String,
    runtime: Option<String>,
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
struct TitleReq {
    message: String,
    linked_context: Option<String>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TranscriptionReq {
    text: String,
    custom_words: Vec<String>,
    strip_fillers: bool,
    llm_polish: bool,
}
impl Default for TranscriptionReq {
    fn default() -> Self {
        Self {
            text: String::new(),
            custom_words: Vec::new(),
            strip_fillers: true,
            llm_polish: false,
        }
    }
}

async fn http_commit(
    State(app): State<Workbench>,
    Json(req): Json<CommitReq>,
) -> std::result::Result<Json<Value>, ApiError> {
    if req.diff.trim().is_empty() {
        return Err(Error::Refused("diff is required".into()).into());
    }
    let root = match req.project_id {
        Some(id) => app.project(id)?.root,
        None => std::env::current_dir().map_err(Error::from)?,
    };
    let message = generate_commit_message(
        &app,
        &root,
        &req.diff,
        req.runtime.as_deref(),
        req.model.as_deref(),
    )
    .await?;
    Ok(Json(json!({"commit_message": message})))
}

async fn http_pr(
    State(app): State<Workbench>,
    Json(req): Json<PrReq>,
) -> std::result::Result<Json<PrContent>, ApiError> {
    let root = match req.project_id {
        Some(id) => app.project(id)?.root,
        None => std::env::current_dir().map_err(Error::from)?,
    };
    let out = generate_pr_content(
        &app,
        &root,
        req.base.as_deref(),
        &req.commit_message,
        &req.diff,
        req.runtime.as_deref(),
        req.model.as_deref(),
    )
    .await;
    Ok(Json(out))
}

async fn http_branch(
    State(app): State<Workbench>,
    Json(req): Json<BranchReq>,
) -> std::result::Result<Json<Value>, ApiError> {
    if req.context.trim().is_empty() {
        return Err(Error::Refused("context is required".into()).into());
    }
    let branch = generate_branch_name(
        Some(&app),
        &req.context,
        req.runtime.as_deref(),
        req.model.as_deref(),
        Uuid::new_v4(),
    )
    .await;
    Ok(Json(json!({"branch_name": branch})))
}

async fn http_title(
    State(app): State<Workbench>,
    Json(req): Json<TitleReq>,
) -> std::result::Result<Json<Value>, ApiError> {
    if req.message.trim().is_empty() {
        return Err(Error::Refused("message is required".into()).into());
    }
    let title = generate_thread_title(&app, &req.message, req.linked_context.as_deref()).await;
    Ok(Json(json!({"title": title})))
}

async fn http_transcription(
    State(app): State<Workbench>,
    Json(req): Json<TranscriptionReq>,
) -> std::result::Result<Json<Value>, ApiError> {
    let text = post_process_transcription(
        Some(&app),
        &req.text,
        &req.custom_words,
        req.strip_fillers,
        req.llm_polish,
    )
    .await;
    Ok(Json(json!({"text": text})))
}

pub(super) fn routes() -> Router<Workbench> {
    Router::new()
        .route("/text-generation/commit-message", post(http_commit))
        .route("/text-generation/pr-content", post(http_pr))
        .route("/text-generation/branch-name", post(http_branch))
        .route("/text-generation/thread-title", post(http_title))
        .route("/text-generation/transcription", post(http_transcription))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::post};
    use tokio::net::TcpListener;

    #[test]
    fn commit_prompt_honors_writing_style_modes_and_custom_instructions() {
        let style = WritingStyle {
            mode: "conventional_commits".into(),
            follow_change_request_templates: true,
            custom_instructions: "Reference ticket IDs when known.".into(),
        };
        let (sys, user) = build_commit_prompt(&style, &["old subject".into()], "+fn hello() {}");
        assert!(sys.contains("Conventional Commits"));
        assert!(sys.contains("Reference ticket IDs when known."));
        assert!(user.contains("+fn hello() {}"));

        let repo_style = WritingStyle::default();
        let (_, repo_user) =
            build_commit_prompt(&repo_style, &["chore(wal): add crc32".into()], "+mod wal;");
        assert!(repo_user.contains("chore(wal): add crc32"));
    }

    #[test]
    fn branch_slug_and_pr_template_and_filler_cleanup_work() {
        let id = Uuid::nil();
        assert_eq!(
            sanitize_branch_slug("feat/Add Pure-Rust WAL!", id),
            "feat/add-pure-rust-wal"
        );
        assert_eq!(
            sanitize_branch_slug("Fix GB18030 decoding", id),
            "kar/fix-gb18030-decoding"
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".github")).unwrap();
        std::fs::write(
            dir.path().join(".github/PULL_REQUEST_TEMPLATE.md"),
            "## Summary\n- \n## Test Plan\n- ",
        )
        .unwrap();
        assert!(
            read_pr_template(dir.path())
                .unwrap()
                .contains("## Test Plan")
        );

        let cleaned = strip_speech_fillers_and_apply_vocab(
            "呃，um we updated karchat and uh ironcalc today!",
            &["KarChat".into(), "IronCalc".into()],
            true,
        );
        assert_eq!(cleaned, "we updated KarChat and IronCalc today!");
    }

    #[tokio::test]
    async fn openai_and_anthropic_http_drivers_parse_responses() {
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post(|| async {
                    Json(json!({
                        "choices": [{"message": {"content": "feat: add text generation"}}]
                    }))
                }),
            )
            .route(
                "/v1/messages",
                post(|| async {
                    Json(json!({
                        "content": [{"type": "text", "text": "fix: anthropic fallback"}]
                    }))
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let oai = HttpDriver::OpenAiCompatible {
            base_url: format!("http://{addr}/v1"),
            api_key: "test-key".into(),
            model: "gpt-4o-mini".into(),
        };
        assert_eq!(
            call_http_driver(&oai, "sys", "prompt").await.unwrap(),
            "feat: add text generation"
        );

        let ant = HttpDriver::Anthropic {
            base_url: format!("http://{addr}"),
            api_key: "test-ant".into(),
            model: "claude-3-5-haiku-latest".into(),
        };
        assert_eq!(
            call_http_driver(&ant, "sys", "prompt").await.unwrap(),
            "fix: anthropic fallback"
        );
    }
}
