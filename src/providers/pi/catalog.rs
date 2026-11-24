use super::PiProviders;
use crate::{
    Error, Result,
    account::read_profile,
    process::OUTPUT_LIMIT,
    providers::{LoginMethod, LoginProvider, Method, ProviderErrorKind},
};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

// Public names and methods from Pi 1.6.0's ModelRuntime with networking and
// credential resolution disabled. Pi's CLI has no read-only provider menu.
const BUILTINS: &str = include_str!("catalog.json");

#[derive(Deserialize)]
struct Models {
    providers: IndexMap<String, Overlay>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Overlay {
    name: Option<String>,
    oauth: Option<String>,
    api_key: Option<String>,
}

impl PiProviders {
    fn builtins(&self) -> Result<Vec<LoginProvider>> {
        if !self
            .version
            .as_deref()
            .and_then(|s| semver::Version::parse(s).ok())
            .is_some_and(|v| (v.major, v.minor) == (1, 6))
        {
            return Err(ProviderErrorKind::VersionUnsupported
                .error("Pi provider metadata requires the Pi 1.6 registry"));
        }
        Ok(serde_json::from_str(BUILTINS)?)
    }

    /// Includes models.json providers and built-in overrides from this Pi profile.
    pub async fn login_providers(&self) -> Result<Vec<LoginProvider>> {
        if let Some(sdk) = self.host().await? {
            return super::within(self.timeout, "Pi SDK provider menu", sdk.menu(self)).await;
        }
        self.terminal_login_providers().await
    }
    pub(super) async fn terminal_login_providers(&self) -> Result<Vec<LoginProvider>> {
        let (rows, custom) = self.registry()?;
        if !custom {
            return Ok(rows);
        }
        // Ask Pi to validate the complete schema and compose models. An absent
        // provider returns before checkAuth, including key commands and refresh.
        let absent = format!("oar-registry-{}", uuid::Uuid::new_v4());
        let check = self
            .run(&[
                "auth",
                "check",
                "--provider",
                &absent,
                "--no-refresh",
                "--json",
            ])
            .await?;
        let state: Value = serde_json::from_slice(&check.stdout)
            .map_err(|_| Error::Protocol("Pi did not confirm its provider configuration".into()))?;
        if check.status.code() != Some(1)
            || state["provider"] != absent
            || state["reason"] != "provider_not_found"
        {
            return Err(Error::Protocol(
                "Pi could not load its provider configuration".into(),
            ));
        }
        Ok(rows)
    }
    pub(super) fn registry(&self) -> Result<(Vec<LoginProvider>, bool)> {
        let rows = self.builtins()?;
        let config = self
            .models_path
            .as_ref()
            .map(|path| read_profile(path, OUTPUT_LIMIT))
            .transpose()?
            .flatten();
        let has_config = config.is_some();
        let config: Option<Models> = config
            .map(|text| {
                serde_json::from_reader(json_comments::StripComments::new(
                    text.trim_start_matches('\u{feff}').as_bytes(),
                ))
            })
            .transpose()
            .map_err(|_| Error::Protocol("Invalid Pi provider configuration".into()))?;
        let mut rows: IndexMap<_, _> = rows
            .into_iter()
            .map(|p| (p.provider_id.clone(), p))
            .collect();
        for (id, overlay) in config.into_iter().flat_map(|c| c.providers) {
            let row = rows.entry(id.clone()).or_insert_with(|| LoginProvider {
                provider_id: id.clone(),
                name: id.clone(),
                methods: Vec::new(),
            });
            if let Some(name) = overlay.name {
                row.name = name;
            } else if overlay.oauth.as_deref() == Some("radius") {
                row.name = id;
            }
            if overlay.oauth.as_deref() == Some("radius") {
                row.methods = vec![
                    LoginMethod {
                        method: Method::Oauth,
                        name: row.name.clone(),
                        ambient: false,
                        subscription: Some(false),
                        login_label: None,
                    },
                    key_method("Radius API key"),
                ];
            } else if row.methods.is_empty()
                || (overlay.api_key.is_some()
                    && !row.methods.iter().any(|m| m.method == Method::ApiKey))
            {
                row.methods.push(key_method("API key"));
            }
        }
        let mut rows: Vec<_> = rows.into_values().collect();
        let order = icu_collator::Collator::try_new(Default::default(), Default::default())
            .map_err(|_| Error::Protocol("Pi provider name collation unavailable".into()))?;
        rows.sort_by(|a, b| order.compare(&a.name, &b.name));
        Ok((rows, has_config))
    }
}

fn key_method(name: &str) -> LoginMethod {
    LoginMethod {
        method: Method::ApiKey,
        name: name.into(),
        ambient: false,
        subscription: None,
        login_label: None,
    }
}
