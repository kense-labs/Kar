//! Static credential declarations from botiverse (0.129.1) and the native Cursor CLI, shared by native and Wasm hosts.
//! Describes configuration by name. Inspecting the declarations performs no I/O.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::LazyLock};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeCredentials {
    pub scope: Scope,
    pub management: Management,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sign_in_paste: Option<Paste>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_directory_env: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interpolation: Vec<Interpolation>,
    pub env: Vec<Variable>,
    pub stores: Vec<Store>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variable {
    pub name: String,
    pub purpose: Purpose,
    pub secret: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<Field>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Store {
    pub kind: StoreKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory_env: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_directory_envs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative_to: Option<RelativeTo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overrides_env: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interpolation {
    pub syntax: String,
    pub target: Target,
}

macro_rules! choices {
    ($case:literal, $name:ident { $($value:ident),+ }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = $case)]
        pub enum $name { $($value),+ }
    }
}
choices!(
    "snake_case",
    Scope {
        SingleAccount,
        PerProvider
    }
);
choices!("snake_case", Management { Native, External });
choices!(
    "snake_case",
    Paste {
        None,
        Code,
        CodeOrUrl
    }
);
choices!(
    "snake_case",
    StoreKind {
        File,
        Dir,
        Keychain
    }
);
choices!("snake_case", RelativeTo { Cwd });
choices!("snake_case", Target { Env, Shell });
choices!(
    "snake_case",
    Purpose {
        ApiKey,
        Token,
        BaseUrl,
        ProviderSelector,
        Region,
        AccountScope,
        ModelRoute,
        CredentialSource,
        Headers
    }
);
choices!(
    "camelCase",
    Field {
        ApiKey,
        AuthToken,
        BaseUrl,
        Headers,
        Region,
        Project,
        Profile,
        Model,
        Provider,
        CredentialSource,
        AccountScope
    }
);

/// Available for runtimes with verified credential declarations for this execution mode.
pub fn for_runtime(id: &str) -> Option<&'static RuntimeCredentials> {
    static DECLARATIONS: LazyLock<BTreeMap<String, RuntimeCredentials>> = LazyLock::new(|| {
        serde_json::from_str(include_str!("account/credentials.json"))
            .expect("checked credential declarations")
    });
    DECLARATIONS.get(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn declarations_match_the_wire_shape() {
        let original: serde_json::Value =
            serde_json::from_str(include_str!("account/credentials.json")).unwrap();
        for (id, shape) in original.as_object().unwrap() {
            assert_eq!(
                &serde_json::to_value(for_runtime(id).unwrap()).unwrap(),
                shape,
                "{id}"
            );
        }
        let claude = for_runtime("claude").unwrap();
        assert!(
            claude
                .env
                .iter()
                .any(|v| v.name == "ANTHROPIC_CUSTOM_HEADERS" && v.secret)
        );
        assert!(for_runtime("pi-durable").unwrap().stores.is_empty());
        assert!(for_runtime("unknown").is_none());
    }
}
