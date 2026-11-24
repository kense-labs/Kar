use super::*;
use serde::Deserialize;

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Names {
    One(String),
    Many(Vec<String>),
}
fn names(input: Option<Names>) -> Vec<String> {
    let all = match input {
        None => vec![],
        Some(Names::One(v)) => v.split(',').map(str::to_owned).collect(),
        Some(Names::Many(a)) => a,
    };
    all.iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
#[derive(Deserialize)]
struct Form {
    name: String,
    description: String,
    #[serde(default)]
    title: String,
    labels: Option<Names>,
    assignees: Option<Names>,
    body: Vec<Field>,
}
#[derive(Deserialize)]
struct Field {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    #[serde(default)]
    attributes: Attributes,
    #[serde(default)]
    validations: Validation,
}
#[derive(Default, Deserialize)]
struct Attributes {
    label: Option<String>,
    description: Option<String>,
    value: Option<String>,
    placeholder: Option<String>,
    render: Option<String>,
    multiple: Option<bool>,
    default: Option<Selection>,
    #[serde(default)]
    options: Vec<Choice>,
}
#[derive(Default, Deserialize)]
struct Validation {
    required: Option<bool>,
    accept: Option<String>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Selection {
    One(i64),
    Many(Vec<i64>),
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Choice {
    Text(String),
    Entry {
        label: String,
        required: Option<bool>,
    },
}
impl Field {
    fn render(self) -> String {
        let a = self.attributes;
        if self.kind == "markdown" {
            return a.value.unwrap_or_default();
        }
        let mut sections = vec![format!(
            "### {}",
            a.label
                .as_deref()
                .or(self.id.as_deref())
                .unwrap_or(&self.kind)
        )];
        if let Some(s) = a.description {
            sections.push(s)
        }
        if self.validations.required == Some(true) {
            sections.push("Required.".into())
        }
        if let Some(s) = a.placeholder.filter(|s| !s.is_empty()) {
            sections.push(format!("Example:\n> {}", s.trim().replace('\n', "\n> ")))
        }
        if let Some(s) = a.render.filter(|s| !s.is_empty()) {
            sections.push(format!("Format: {s}"))
        }
        if let Some(s) = self.validations.accept.filter(|s| !s.is_empty()) {
            sections.push(format!("Accepted files: {s}"))
        }
        if matches!(self.kind.as_str(), "checkboxes" | "dropdown") {
            let dropdown = self.kind == "dropdown";
            if dropdown {
                sections.push(
                    if a.multiple == Some(true) {
                        "Choose one or more:"
                    } else {
                        "Choose one:"
                    }
                    .into(),
                )
            }
            let selected = match a.default {
                Some(Selection::One(n)) => vec![n],
                Some(Selection::Many(ns)) => ns,
                None => vec![],
            };
            let lines = a
                .options
                .into_iter()
                .enumerate()
                .map(|(i, o)| {
                    let (label, required) = match o {
                        Choice::Text(v) => (v, false),
                        Choice::Entry { label, required } => (label, required == Some(true)),
                    };
                    if dropdown {
                        format!(
                            "- {label}{}",
                            if selected.contains(&(i as i64)) {
                                " (default)"
                            } else {
                                ""
                            }
                        )
                    } else {
                        format!("- [ ] {label}{}", if required { " (required)" } else { "" })
                    }
                })
                .collect::<Vec<_>>();
            sections.push(lines.join("\n"));
        }
        if let Some(s) = a.value {
            sections.push(s)
        }
        sections.retain(|s| !s.is_empty());
        sections.join("\n\n")
    }
}
pub(super) fn github(repo: &Value) -> Result<Value, ForgeError> {
    let mut templates = vec![];
    let mut positions = std::collections::HashMap::new();
    let mut insert = |key: &str, row: Value| {
        let i = positions.entry(key.to_owned()).or_insert(templates.len());
        if *i == templates.len() {
            templates.push(row)
        } else {
            templates[*i] = row
        }
    };
    for template in repo["issueTemplates"].as_array().into_iter().flatten() {
        let key = text(template, "filename");
        if key.trim().is_empty() {
            continue;
        }
        let labels = template["labels"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["name"].as_str())
            .collect::<Vec<_>>();
        let assignees = template["assignees"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["login"].as_str())
            .collect::<Vec<_>>();
        insert(
            key.rsplit('/').next().unwrap_or(key),
            json!({"key":key,"name":template["name"],"about":text(template,"about"),"title":text(template,"title"),"body":text(template,"body"),"labels":labels,"assignees":assignees}),
        );
    }
    let mut blank = true;
    for entry in repo["templateDirectory"]["entries"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let key = text(entry, "name");
        let lower = key.to_lowercase();
        if !lower.ends_with(".yaml") && !lower.ends_with(".yml") {
            continue;
        }
        let blob = &entry["object"];
        let content = blob["text"]
            .as_str()
            .filter(|_| blob["__typename"] == "Blob" && blob["isTruncated"] != true)
            .ok_or_else(|| invalid("incomplete GitHub template blob"))?;
        if matches!(lower.as_str(), "config.yaml" | "config.yml") {
            #[derive(Deserialize, Default)]
            struct Config {
                blank_issues_enabled: Option<bool>,
            }
            let cfg = serde_saphyr::from_str::<Option<Config>>(content)
                .map_err(|_| invalid("GitHub template configuration"))?
                .unwrap_or_default();
            blank = cfg.blank_issues_enabled.unwrap_or(true);
        } else if let Ok(form) = serde_saphyr::from_str::<Form>(content)
            && !form.name.trim().is_empty()
        {
            let body = form
                .body
                .into_iter()
                .map(Field::render)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            insert(
                key,
                json!({"key":key,"name":form.name,"about":form.description,"title":form.title,"body":body,"labels":names(form.labels),"assignees":names(form.assignees)}),
            );
        }
    }
    Ok(json!({"templates":templates,"blankIssuesEnabled":blank}))
}

pub(super) async fn rest(c: &Context, base: Url) -> Result<Value, ForgeError> {
    if c.kind == "forgejo" {
        let data = get(c, child(base.clone(), &["issue_templates"])).await?;
        let config = match get(c, child(base, &["issue_config"])).await {
            Ok(v) => v,
            Err(e) if e.status == 404 || e.status >= 500 => Value::Null,
            Err(e) => return Err(e),
        };
        let result=rows(&data,"Forgejo templates")?.iter().filter_map(|v|{let key=text(v,"file_name").trim();if !key.to_ascii_lowercase().ends_with(".md"){return None}
            Some(json!({"key":key,"name":v["name"].as_str().map(str::trim).filter(|s|!s.is_empty()).unwrap_or(key),"about":text(v,"about"),"title":text(v,"title"),"body":text(v,"content"),"labels":v["labels"].as_array().cloned().unwrap_or_default(),"assignees":v["assignees"].as_array().cloned().unwrap_or_default()}))
        }).collect::<Vec<_>>();
        return Ok(
            json!({"templates":result,"blankIssuesEnabled":config["blank_issues_enabled"]!=false}),
        );
    }
    let base = child(base, &["templates", "issues"]);
    let list = get(c, base.clone()).await?;
    let mut result = vec![];
    for item in rows(&list, "GitLab issue template index")?.iter().take(20) {
        let key = item["key"]
            .as_str()
            .ok_or_else(|| invalid("GitLab template key"))?;
        let data = get(c, child(base.clone(), &[key])).await?;
        result.push(json!({"key":key,"name":item["name"].as_str().unwrap_or(key),"about":"","title":"","body":text(&data,"content"),"labels":[],"assignees":[]}));
    }
    Ok(json!({"templates":result,"blankIssuesEnabled":true}))
}

pub(super) fn linear(team: &Value, key: &str, labels: Vec<&Value>) -> Result<Value, ForgeError> {
    let mut templates = vec![];
    let mut external = vec![];
    for row in rows(&team["templates"]["nodes"], "Linear templates")?
        .iter()
        .filter(|r| r["type"] == "issue")
    {
        let id = row["id"]
            .as_str()
            .ok_or_else(|| invalid("Linear template ID"))?;
        let data = &row["templateData"];
        let body = row["content"]
            .as_str()
            .or_else(|| data["description"].as_str());
        if row["hasFormFields"] == true || body.is_none() && !data["descriptionData"].is_null() {
            let mut url = Url::parse("https://linear.app/").unwrap();
            url.path_segments_mut()
                .unwrap()
                .extend(["team", key, "new"]);
            url.query_pairs_mut().append_pair("template", id);
            external.push(
                json!({"name":row["name"],"about":text(row,"description"),"url":url.as_str()}),
            );
        } else {
            let names = data["labelIds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| {
                    labels
                        .iter()
                        .find(|l| l["id"] == *id)?
                        .get("name")?
                        .as_str()
                })
                .collect::<Vec<_>>();
            templates.push(json!({"key":id,"name":row["name"],"about":text(row,"description"),"body":body.unwrap_or(""),"title":text(data,"title"),"labels":names,"assignees":data["assigneeId"].as_str().into_iter().collect::<Vec<_>>()}));
        }
    }
    Ok(json!({"templates":templates,"externalTemplates":external,"blankIssuesEnabled":true}))
}
