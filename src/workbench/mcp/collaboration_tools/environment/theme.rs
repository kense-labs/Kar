use super::*;
const BUILTIN: &[&str] = &["kar", "kar-chat", "grove", "ocean", "ember", "iris"];
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !matches!(id, "system" | "light" | "dark")
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Theme {
    version: Option<u8>,
    name: String,
    appearance: String,
    canvas: Option<String>,
    accent: Option<String>,
    colors: Option<BTreeMap<String, String>>,
    variants: Option<BTreeMap<String, BTreeMap<String, String>>>,
    css: Option<String>,
}
fn palette(colors: &BTreeMap<String, String>) -> bool {
    colors.len() <= 256
        && colors.iter().all(|(k, v)| {
            k.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                && k.len() <= 64
                && k.bytes().all(|b| b.is_ascii_alphanumeric())
                && !v.trim().is_empty()
                && v.len() <= 64
        })
}
fn validate(t: &Theme) -> bool {
    t.version.unwrap_or(1) == 1
        && !t.name.trim().is_empty()
        && t.name.chars().count() <= 48
        && matches!(t.appearance.as_str(), "light" | "dark")
        && [&t.canvas, &t.accent].into_iter().flatten().all(|s| {
            s.strip_prefix('#').is_some_and(|c| {
                matches!(c.len(), 3 | 6) && c.bytes().all(|b| b.is_ascii_hexdigit())
            })
        })
        && t.colors.as_ref().is_none_or(palette)
        && t.variants.as_ref().is_none_or(|v| {
            v.iter()
                .all(|(k, p)| matches!(k.as_str(), "light" | "dark") && palette(p))
        })
        && t.css.as_ref().is_none_or(|s| s.len() <= 100000)
}
pub(super) fn read(app: &Workbench, id: Option<&str>) -> Result<Value> {
    let db = app.0.catalog.lock().unwrap();
    let env = &db.environment;
    let mut result = json!({"built_in":BUILTIN,"current":{"theme_id":env.selected_theme.as_deref().unwrap_or("kar"),"appearance":env.appearance.as_deref().unwrap_or("system")},
        "published":env.themes.iter().map(|(k,v)|json!({"id":k,"name":v["name"],"has_css":v["css"].as_str().is_some_and(|s|!s.is_empty())})).collect::<Vec<_>>(),
        "guide":"Choose a built-in ID or publish a named light/dark theme. Set canvas and accent to hex colors; colors and light/dark variants override the palette. Use css for client styles."});
    if let Some(id) = id {
        result["theme"] = env
            .themes
            .get(id)
            .ok_or_else(|| bad("unknown published theme"))?
            .clone();
    }
    Ok(result)
}
pub(super) fn set(
    app: &Workbench,
    caller: Option<(&Caller, &Grant)>,
    input: Value,
) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Set {
        theme_id: Option<String>,
        theme: Option<Value>,
        appearance: Option<String>,
    }
    let m: Set = serde_json::from_value(input)?;
    if m.theme_id.is_none() && m.appearance.is_none()
        || m.appearance
            .as_deref()
            .is_some_and(|s| !matches!(s, "light" | "dark" | "system"))
        || m.theme_id.as_deref().is_some_and(|s| !valid_id(s))
    {
        return Err(bad("invalid theme ID or appearance"));
    }
    if let Some(doc) = &m.theme {
        let theme: Theme = serde_json::from_value(doc.clone())?;
        if !validate(&theme) || m.theme_id.as_deref().is_none_or(|k| BUILTIN.contains(&k)) {
            return Err(bad("invalid published theme"));
        }
    }
    app.update(|db|{
        if let Some((c,g))=caller{check(db,c,g)?;}
        let env=&mut db.environment;
        if let Some(id)=&m.theme_id{
            if let Some(doc)=&m.theme{
                if env.themes.len()>=128&&!env.themes.contains_key(id){return Err(bad("theme library is full"));}
                env.themes.insert(id.clone(),doc.clone());
            }else if !BUILTIN.contains(&id.as_str())&&!env.themes.contains_key(id){return Err(bad("unknown theme"));}
            env.selected_theme=Some(id.clone());
        }
        if let Some(a)=m.appearance{env.appearance=Some(a);}
        Ok(json!({"theme_id":env.selected_theme,"appearance":env.appearance,"published":m.theme.is_some()}))
    })
}
