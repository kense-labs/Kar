use super::*;
use std::sync::LazyLock;
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Rule {
    pub key: String,
    pub command: String,
    pub when: Option<String>,
}
#[derive(Deserialize)]
struct Defaults {
    commands: Vec<String>,
    rules: Vec<Rule>,
}
static DEFAULTS: LazyLock<Defaults> = LazyLock::new(|| {
    serde_json::from_str(include_str!("shortcuts.json")).expect("bundled shortcut definitions")
});
fn shortcut(raw: &str) -> Option<Value> {
    if raw.trim().is_empty() || raw.len() > 64 {
        return None;
    }
    let lower = raw.to_lowercase();
    let mut parts = lower.split('+').map(str::trim).collect::<Vec<_>>();
    if parts.last() == Some(&"") {
        while parts.last() == Some(&"") {
            parts.pop();
        }
        parts.push("+");
    }
    let mut result =
        json!({"metaKey":false,"ctrlKey":false,"altKey":false,"shiftKey":false,"modKey":false});
    for v in parts {
        match v {
            "meta" | "cmd" => result["metaKey"] = true.into(),
            "ctrl" | "control" => result["ctrlKey"] = true.into(),
            "alt" | "option" => result["altKey"] = true.into(),
            "shift" => result["shiftKey"] = true.into(),
            "mod" => result["modKey"] = true.into(),
            "" => return None,
            key if result.get("key").is_none() => {
                result["key"] = match key {
                    "space" => " ",
                    "esc" => "escape",
                    k => k,
                }
                .into()
            }
            _ => return None,
        }
    }
    result.get("key").is_some().then_some(result)
}
struct Parser<'a>(&'a str);
impl Parser<'_> {
    fn eat(&mut self, part: &str) -> bool {
        self.0 = self.0.trim_start();
        if let Some(rest) = self.0.strip_prefix(part) {
            self.0 = rest;
            true
        } else {
            false
        }
    }
    fn expr(&mut self, level: u8, depth: u8) -> Option<Value> {
        if depth > 64 {
            return None;
        }
        if level < 2 {
            let (op, kind) = if level == 0 {
                ("||", "or")
            } else {
                ("&&", "and")
            };
            let mut node = self.expr(level + 1, depth)?;
            while self.eat(op) {
                node = json!({"type":kind,"left":node,"right":self.expr(level+1,depth)?})
            }
            return Some(node);
        }
        if self.eat("!") {
            return Some(json!({"type":"not","node":self.expr(2,depth+1)?}));
        }
        if self.eat("(") {
            let node = self.expr(0, depth + 1)?;
            return self.eat(")").then_some(node);
        }
        let first = *self.0.as_bytes().first()?;
        if !first.is_ascii_alphabetic() && first != b'_' {
            return None;
        }
        let n = self
            .0
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || b"_.-".contains(b))
            .count();
        let (key, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(json!({"type":"identifier","name":key}))
    }
}
impl Rule {
    fn resolved(&self) -> Result<Value> {
        let valid_script = self
            .command
            .strip_prefix("script.")
            .and_then(|s| s.strip_suffix(".run"))
            .is_some_and(|s| {
                !s.is_empty()
                    && s.len() <= 24
                    && s.as_bytes()[0] != b'-'
                    && s.bytes()
                        .all(|v| v.is_ascii_lowercase() || v.is_ascii_digit() || v == b'-')
            });
        if !DEFAULTS.commands.contains(&self.command) && !valid_script {
            return Err(bad("Unknown keyboard command"));
        }
        let mut result = json!({"command":self.command,"shortcut":shortcut(&self.key).ok_or_else(||bad("Invalid keyboard shortcut"))?});
        if let Some(when) = &self.when {
            if when.len() > 256 {
                return Err(bad("Shortcut condition exceeds 256 characters"));
            }
            let mut parser = Parser(when.trim());
            let node = parser
                .expr(0, 0)
                .filter(|_| parser.0.trim().is_empty())
                .ok_or_else(|| bad("Invalid shortcut condition"))?;
            result["whenAst"] = node;
        }
        Ok(result)
    }
}
pub(super) fn resolved(custom: &[Rule]) -> Result<Vec<Value>> {
    let mut out = DEFAULTS
        .rules
        .iter()
        .filter(|rule| !custom.iter().any(|r| r.command == rule.command))
        .chain(custom)
        .map(Rule::resolved)
        .collect::<Result<Vec<_>>>()?;
    if out.len() > 256 {
        out.drain(..out.len() - 256);
    }
    Ok(out)
}
#[derive(Deserialize)]
pub(super) struct Upsert {
    key: String,
    command: String,
    when: Option<String>,
    replace: Option<Rule>,
}
fn change(db: &mut Catalog, r: Rule, replace: Option<Rule>) -> Result<()> {
    let target = r.resolved()?;
    let old = replace.as_ref().map(Rule::resolved).transpose()?;
    let rules = &mut db.server.bindings;
    if !rules.iter().any(|v| v.command == r.command)
        && replace
            .as_ref()
            .is_none_or(|v| v.command == r.command && DEFAULTS.rules.contains(v))
    {
        for d in DEFAULTS.rules.iter().filter(|d| d.command == r.command) {
            let current = d.resolved()?;
            if Some(&current) != old.as_ref() && current != target {
                rules.push(d.clone())
            }
        }
    }
    rules.retain(|v| {
        v.resolved()
            .is_ok_and(|v| v != target && old.as_ref() != Some(&v))
    });
    if rules.len() >= 256 {
        return Err(bad("At most 256 custom shortcut rules are supported"));
    }
    rules.push(r);
    Ok(())
}
pub(super) async fn upsert(State(app): State<Workbench>, Json(p): Json<Upsert>) -> Reply {
    let r = Rule {
        key: p.key,
        command: p.command,
        when: p.when,
    };
    Ok(Json(app.update(|db| {
        change(db, r, p.replace)?;
        Ok(json!({"keybindings":resolved(&db.server.bindings)?,"issues":[]}))
    })?))
}
pub(super) async fn remove(State(app): State<Workbench>, Json(p): Json<Rule>) -> Reply {
    let target = p.resolved()?;
    Ok(Json(app.update(|db| {
        db.server
            .bindings
            .retain(|v| v.resolved().is_ok_and(|v| v != target));
        Ok(json!({"keybindings":resolved(&db.server.bindings)?,"issues":[]}))
    })?))
}
