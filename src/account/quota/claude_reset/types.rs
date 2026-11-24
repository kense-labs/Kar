use super::super::{InstallationOptions, LimitResetResult};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Default)]
pub struct LimitResetInspectOptions {
    pub installation: InstallationOptions,
    pub timeout: Option<Duration>,
    pub five_hour_limit_consent: bool,
    pub include_five_hour: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitResetReport {
    #[serde(flatten)]
    pub result: LimitResetResult,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub onboarding_completed: bool,
}
impl From<LimitResetResult> for LimitResetReport {
    fn from(result: LimitResetResult) -> Self {
        Self {
            result,
            onboarding_completed: false,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetKind {
    FiveHour,
    Weekly,
    Fable,
    Full,
}
impl ResetKind {
    pub(super) const ALL: [Self; 4] = [Self::FiveHour, Self::Weekly, Self::Fable, Self::Full];
    pub(in super::super) fn parse(s: &str) -> Option<Self> {
        match s {
            "five_hour" => Some(Self::FiveHour),
            "weekly" => Some(Self::Weekly),
            "fable" => Some(Self::Fable),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetState {
    Available,
    None,
    NotOffered,
    Unknown,
    NotChecked,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetProgram {
    Legacy,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetKindStatus {
    pub id: ResetKind,
    pub state: ResetState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_left: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_text: Option<String>,
}
impl ResetKindStatus {
    pub(super) fn new(id: ResetKind, state: ResetState) -> Self {
        Self {
            id,
            state,
            resets_left: None,
            expires_text: None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotAttemptedReason {
    Busy,
    Deadline,
    UsageUnread,
    StartupFailed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupDiagnostic {
    pub screen: StartupScreen,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupScreen {
    Blank,
    Exited,
    Onboarding,
    TrustUnrecognised,
    Dialog,
    PromptWithText,
    NoBanner,
    Unsettled,
    Unknown,
}
