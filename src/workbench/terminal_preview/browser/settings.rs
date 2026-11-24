use crate::{Error, Result};
use serde::{Deserialize, Serialize};

#[derive(Default, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "_tag",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(in super::super) enum Viewport {
    #[default]
    Fill,
    Freeform {
        width: u32,
        height: u32,
    },
    Preset {
        width: u32,
        height: u32,
        preset_id: String,
    },
}
impl Viewport {
    pub(super) fn pixels(&self) -> Result<(u32, u32)> {
        let (w, h) = match self {
            Self::Fill => return Ok((1280, 800)),
            Self::Freeform { width, height } => (*width, *height),
            Self::Preset {
                width,
                height,
                preset_id,
            } => {
                let valid = [
                    "iphone-se",
                    "iphone-xr",
                    "iphone-12-pro",
                    "iphone-14-pro-max",
                    "pixel-7",
                    "samsung-galaxy-s8-plus",
                    "samsung-galaxy-s20-ultra",
                    "ipad-mini",
                    "ipad-air",
                    "ipad-pro",
                    "surface-pro-7",
                    "surface-duo",
                    "galaxy-z-fold-5",
                    "asus-zenbook-fold",
                    "samsung-galaxy-a51-71",
                    "nest-hub",
                    "nest-hub-max",
                    "desktop-1920x1080",
                    "desktop-1440x900",
                    "laptop-1366x768",
                    "laptop-1280x800",
                    "ipad-pro-11",
                    "iphone-15-pro",
                    "pixel-8",
                    "galaxy-s24",
                ];
                if !valid.contains(&preset_id.as_str()) {
                    return Err(Error::Refused("unknown preview viewport preset".into()));
                }
                (*width, *height)
            }
        };
        if !(240..=3840).contains(&w) || !(240..=3840).contains(&h) || w * h > 3840 * 2160 {
            return Err(Error::Refused("invalid preview viewport dimensions".into()));
        }
        Ok((w, h))
    }
}
#[derive(Default, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(in super::super) enum Color {
    #[default]
    System,
    Light,
    Dark,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub(in super::super) struct Display {
    pub viewport: Viewport,
    pub zoom_factor: f64,
    pub color_scheme: Color,
}
impl Default for Display {
    fn default() -> Self {
        Self {
            viewport: Viewport::Fill,
            zoom_factor: 1.0,
            color_scheme: Color::System,
        }
    }
}
impl Display {
    pub(in super::super) fn validate(&self) -> Result<()> {
        self.viewport.pixels()?;
        if ![
            0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0,
            5.0,
        ]
        .contains(&self.zoom_factor)
        {
            return Err(Error::Refused("unsupported preview zoom factor".into()));
        }
        Ok(())
    }
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in super::super) struct Adjust {
    pub zoom_factor: Option<f64>,
    pub color_scheme: Option<Color>,
    #[serde(default)]
    pub hard_reload: bool,
    pub clear: Option<Clear>,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(in super::super) enum Clear {
    Cookies,
    Cache,
}
pub(in super::super) fn profile(id: &str) -> Result<()> {
    if id.is_empty()
        || id != id.trim()
        || id.chars().count() > 64
        || id.chars().any(char::is_control)
    {
        return Err(Error::Refused("invalid browser profile identifier".into()));
    }
    Ok(())
}
