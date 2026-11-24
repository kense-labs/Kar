//! Machine-readable description of the same commands the native installer runs.
use super::InstallPlan;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct InstallCommand {
    pub program: &'static str,
    pub arguments: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstallPreview {
    #[serde(flatten)]
    pub plan: InstallPlan,
    pub source: String,
    pub network: bool,
    /// Kar starts these commands as the current user.
    pub privileges: &'static str,
    pub steps: Vec<InstallCommand>,
}

const CURL_FETCH: &[&str] = &[
    "--fail",
    "--silent",
    "--show-error",
    "--location",
    "--proto",
    "=https",
    "--proto-redir",
    "=https",
    "--output",
];
const TEMP_SCRIPT: &str = "<temporary installer file>";

impl InstallPlan {
    pub fn preview(&self) -> InstallPreview {
        let (source, steps) = match self {
            Self::Npm { package } => (
                format!("https://www.npmjs.com/package/{package}"),
                vec![InstallCommand {
                    program: npm_program(),
                    arguments: ["install", "--global", *package]
                        .into_iter()
                        .map(String::from)
                        .collect(),
                }],
            ),
            Self::VendorScript {
                url,
                interpreter,
                args,
            } => {
                let mut fetch: Vec<String> = CURL_FETCH.iter().copied().map(String::from).collect();
                fetch.extend([TEMP_SCRIPT.to_owned(), url.to_string()]);
                (
                    url.to_string(),
                    vec![
                        InstallCommand {
                            program: "curl",
                            arguments: fetch,
                        },
                        InstallCommand {
                            program: interpreter,
                            arguments: std::iter::once(TEMP_SCRIPT)
                                .chain(args.iter().copied())
                                .map(String::from)
                                .collect(),
                        },
                    ],
                )
            }
        };
        InstallPreview {
            plan: self.clone(),
            source,
            network: true,
            privileges: "current_user",
            steps,
        }
    }
}
pub(super) fn download_args() -> &'static [&'static str] {
    CURL_FETCH
}
pub(super) fn npm_program() -> &'static str {
    if cfg!(windows) { "npm.cmd" } else { "npm" }
}
