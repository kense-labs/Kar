//! Offline provider identification. Asset origins and licenses are in NOTICE.md.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::LazyLock};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeBrand {
    pub name: String,
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<ThemeIcons>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThemeIcons {
    pub light: String,
    pub dark: String,
}
#[derive(Clone, Copy, Debug)]
pub enum Background {
    Light,
    Dark,
}

impl RuntimeBrand {
    pub fn icon_for(&self, background: Background) -> Option<&str> {
        self.icons
            .as_ref()
            .map(|icons| match background {
                Background::Light => icons.light.as_str(),
                Background::Dark => icons.dark.as_str(),
            })
            .or(self.icon.as_deref())
    }
}

pub fn runtime_brands() -> &'static BTreeMap<String, RuntimeBrand> {
    static BRANDS: LazyLock<BTreeMap<String, RuntimeBrand>> = LazyLock::new(|| {
        let mut brands: BTreeMap<String, RuntimeBrand> =
            serde_json::from_str(include_str!("catalog.json")).expect("checked runtime brands");
        let mut durable = brands["pi"].clone();
        durable.name = "Pi Durable".into();
        brands.insert("pi-durable".into(), durable);
        brands
    });
    &BRANDS
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimePlatform {
    Darwin,
    Linux,
    Win32,
}

/// Native-program CI declarations from the pinned upstream runtime release.
/// Kar's own acceptance runs and platform results are tracked separately.
pub fn upstream_verified_platforms(id: &str) -> Option<&'static [RuntimePlatform]> {
    use RuntimePlatform::*;
    match id {
        "claude" | "codex" | "qwen" | "qoder" | "dsh" | "antigravity" | "pi" | "pi-durable"
        | "opencode" => Some(&[Linux, Darwin, Win32]),
        "kimi" | "grok" | "factory" | "codebuddy" => Some(&[Linux, Darwin]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_registered_brand_has_offline_identification_and_stable_theme_choices() {
        for (id, brand) in runtime_brands() {
            assert!(!brand.name.is_empty());
            let plain = brand.icon.as_deref().unwrap();
            assert!(plain.starts_with("data:image/svg+xml,"), "{id}");
            for theme in [Background::Light, Background::Dark] {
                assert!(
                    brand
                        .icon_for(theme)
                        .unwrap()
                        .starts_with("data:image/svg+xml,")
                );
                if brand.icons.is_none() {
                    assert_eq!(brand.icon_for(theme), Some(plain));
                }
            }
        }
        assert_eq!(
            runtime_brands()["pi-durable"].icon,
            runtime_brands()["pi"].icon
        );
        assert_eq!(runtime_brands()["pi-durable"].name, "Pi Durable");
    }
}
