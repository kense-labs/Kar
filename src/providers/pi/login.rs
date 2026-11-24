use super::{PiProviders, terminal::Terminal};
use crate::{
    Error, Result,
    account::LoginCancellation,
    providers::{
        Choice, LoginMethod, Method, Prompt, ProviderErrorKind as Kind, ProviderEvent,
        ProviderInteraction, ProviderStatus, interruptible, within,
    },
};
use reqwest::Url;

struct KeyOnly<'a>(Option<&'a str>);
impl ProviderInteraction for KeyOnly<'_> {
    fn event(&mut self, _: ProviderEvent) -> Result<()> {
        Ok(())
    }
    async fn prompt(&mut self, p: Prompt) -> Result<String> {
        if matches!(p, Prompt::Secret { .. })
            && let Some(key) = self.0.take()
        {
            return Ok(key.to_owned());
        }
        Err(Kind::InteractionFailed
            .error("This Pi provider needs more than a key; use interactive login"))
    }
}

impl PiProviders {
    pub async fn set_api_key(&self, id: &str, key: &str) -> Result<()> {
        if key.is_empty() {
            return Err(Kind::Rejected.error("Empty Pi API key"));
        }
        let mut input = KeyOnly(Some(key));
        self.authenticate(
            id,
            Method::ApiKey,
            &mut input,
            &Default::default(),
            &LoginCancellation::default(),
        )
        .await?;
        if input.0.is_some() {
            return Err(Kind::Rejected.error("Pi completed without requesting the API key"));
        }
        Ok(())
    }
    pub async fn login(
        &self,
        id: &str,
        method: Method,
        host: &mut impl ProviderInteraction,
        cancel: &LoginCancellation,
    ) -> Result<ProviderStatus> {
        self.login_with_options(id, method, host, cancel, &Default::default())
            .await
    }
    pub async fn login_with_options(
        &self,
        id: &str,
        method: Method,
        host: &mut impl ProviderInteraction,
        cancel: &LoginCancellation,
        options: &super::PiLoginOptions<'_>,
    ) -> Result<ProviderStatus> {
        self.authenticate(id, method, host, options, cancel).await?;
        interruptible(cancel, self.status(id)).await
    }
    async fn authenticate(
        &self,
        id: &str,
        method: Method,
        host: &mut impl ProviderInteraction,
        options: &super::PiLoginOptions<'_>,
        cancel: &LoginCancellation,
    ) -> Result<()> {
        if let Some(sdk) = interruptible(cancel, self.host()).await? {
            return sdk.login(self, id, method, host, options, cancel).await;
        }
        if options.get_device_id.is_some() || options.agent_name.is_some() {
            return Err(Kind::UnsupportedInstallation.error("Pi host login identity requires the installed npm SDK and a Node host with native addon support"));
        }
        interruptible(
            cancel,
            within(
                self.login_timeout,
                "Pi login",
                self.terminal_login(id, method, host),
            ),
        )
        .await
    }
    async fn terminal_login(
        &self,
        id: &str,
        method: Method,
        host: &mut impl ProviderInteraction,
    ) -> Result<()> {
        if id.is_empty()
            || id.starts_with('-')
            || id.chars().any(char::is_control)
            || id.trim() != id
        {
            return Err(Kind::UnknownProvider.error("Invalid Pi provider ID"));
        }
        let providers = self.terminal_login_providers().await?;
        let provider = providers
            .iter()
            .find(|p| p.provider_id == id)
            .ok_or_else(|| Kind::UnknownProvider.error("Unknown Pi provider"))?;
        // Pi's terminal accepts a case-insensitive display name or ID and
        // chooses the first match. A collision must not target another account.
        if providers.iter().any(|p| {
            p.provider_id != id
                && (p.provider_id.to_lowercase() == id.to_lowercase()
                    || p.name.to_lowercase() == id.to_lowercase())
        }) {
            return Err(Kind::Rejected.error("Ambiguous Pi provider ID"));
        }
        let selected = provider
            .methods
            .iter()
            .find(|m| m.method == method && !m.ambient)
            .ok_or_else(|| Kind::MethodUnavailable.error("Pi does not offer this login method"))?;
        let flag = if method == Method::ApiKey {
            "--api-key"
        } else {
            "--no-browser"
        };
        let mut terminal = Terminal::start(self, &["auth", "login", id, flag, "--offline"])?;
        converse(&mut terminal, &mut Decoder::new(id, selected), host).await?;
        if !self
            .stored()
            .await?
            .0
            .iter()
            .any(|(p, m)| p == id && *m == method)
        {
            return Err(Kind::Rejected.error("Pi did not confirm the requested credential"));
        }
        Ok(())
    }
}

async fn finish(exit: &mut tokio::sync::watch::Receiver<Option<bool>>) -> Result<()> {
    let result = exit
        .wait_for(|s| s.is_some())
        .await
        .map_err(|_| Error::Closed("Pi login process stopped".into()))?;
    if *result == Some(true) {
        Ok(())
    } else {
        Err(Kind::Rejected.error("Pi did not complete provider login"))
    }
}
async fn converse(
    t: &mut Terminal,
    decoder: &mut Decoder<'_>,
    host: &mut impl ProviderInteraction,
) -> Result<()> {
    loop {
        let bytes = tokio::select! {
            biased;
            bytes=t.output.recv()=>bytes,
            result=finish(&mut t.exit)=>{decoder.publish_url(host,None)?;return result},
            _=tokio::time::sleep(std::time::Duration::from_millis(50)), if decoder.pending_url.is_some()=>{
                decoder.publish_url(host,None)?;
                continue;
            }
        };
        let Some(bytes) = bytes else {
            decoder.publish_url(host, None)?;
            return finish(&mut t.exit).await;
        };
        decoder.feed(&bytes?, host)?;
        if std::mem::take(&mut decoder.cursor_query) {
            t.write("\x1b[1;1R".into())?;
        }
        if let Some(prompt) = decoder.prompt()? {
            decoder.publish_url(host, None)?;
            let choices = match &prompt {
                Prompt::Select { options, .. } => {
                    Some(options.iter().map(|c| c.id.clone()).collect::<Vec<_>>())
                }
                _ => None,
            };
            let secret = matches!(prompt, Prompt::Secret { .. });
            let answer = tokio::select! {biased; result=finish(&mut t.exit)=>return result, answer=host.prompt(prompt)=>answer?};
            let native = if let Some(choices) = choices {
                let i = choices
                    .iter()
                    .position(|id| *id == answer)
                    .ok_or_else(|| Kind::InteractionFailed.error("Unknown Pi provider option"))?;
                (i + 1).to_string()
            } else if secret && decoder.method.method == Method::ApiKey {
                // Pi treats a leading ! and embedded $ as config references.
                crate::pi_sdk::config_literal(&answer)
            } else {
                answer
            };
            t.answer(native)?;
            decoder.answered = true;
            decoder.line.clear();
        }
    }
}

struct Decoder<'a> {
    id: &'a str,
    method: &'a LoginMethod,
    line: Vec<u8>,
    escape: u8,
    cursor_query: bool,
    answered: bool,
    url_next: bool,
    pending_url: Option<String>,
    options: Vec<String>,
}
impl<'a> Decoder<'a> {
    fn new(id: &'a str, method: &'a LoginMethod) -> Self {
        Self {
            id,
            method,
            line: vec![],
            escape: 0,
            cursor_query: false,
            answered: false,
            url_next: false,
            pending_url: None,
            options: vec![],
        }
    }
    fn feed(&mut self, bytes: &[u8], host: &mut impl ProviderInteraction) -> Result<()> {
        for &b in bytes {
            match (self.escape, b) {
                (0, 0x1b) => self.escape = 1,
                (1, b'[') => self.escape = 2,
                (1, b']') => self.escape = 3,
                (2, b'6') => self.escape = 5,
                (5, b'n') => {
                    self.escape = 0;
                    self.cursor_query = true;
                }
                (5, 0x30..=0x3f) => self.escape = 2,
                (1, _) | (2 | 5, 0x40..=0x7e) | (3, 7) | (4, b'\\') => self.escape = 0,
                (3, 0x1b) => self.escape = 4,
                (4, _) => self.escape = 3,
                (0, b'\n') => {
                    if !self.answered {
                        let line = std::mem::take(&mut self.line);
                        self.complete_line(&String::from_utf8_lossy(&line), host)?;
                    }
                    self.answered = false;
                    self.line.clear();
                }
                (0, 32..=255) if !self.answered => {
                    if self.line.len() >= 65536 {
                        return Err(Error::Protocol("Oversized Pi terminal line".into()));
                    }
                    self.line.push(b);
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn complete_line(&mut self, line: &str, host: &mut impl ProviderInteraction) -> Result<()> {
        if self.method.method == Method::ApiKey {
            if let Some((n, label)) = line.trim_start().split_once(". ")
                && n.parse::<usize>() == Ok(self.options.len() + 1)
                && self.options.len() < 16
            {
                self.options.push(label.into());
            }
            return Ok(());
        }
        if self.url_next {
            self.url_next = false;
            self.pending_url = Some(http_url(line.trim())?.into());
            return Ok(());
        }
        if self.pending_url.is_some() {
            // Pi prints optional browser instructions as a separate terminal
            // write. Keep error output and all input echoes private.
            let instructions = line.starts_with("Complete login in your browser")
                || line.starts_with("Complete sign-in in your browser")
                || matches!(
                    line,
                    "Continue in your browser."
                        | "A browser window should open. Complete login to finish."
                );
            self.publish_url(host, instructions.then_some(line))?;
            if instructions {
                return Ok(());
            }
        }
        if line == "Open this URL to sign in:" {
            self.url_next = true;
        } else if let Some((uri, code)) = line
            .strip_prefix("Open ")
            .and_then(|s| s.split_once(" and enter code: "))
        {
            if code.is_empty() || code.len() > 256 || code.chars().any(char::is_whitespace) {
                return Err(Error::Protocol("Invalid Pi pairing code".into()));
            }
            host.event(ProviderEvent::DeviceCode {
                interval_seconds: None,
                verification_uri: http_url(uri)?.into(),
                user_code: code.into(),
                expires_in_seconds: None,
            })?;
        } else if matches!(
            line,
            "Waiting for authentication..."
                | "Exchanging authorization code for tokens..."
                | "Exchanging authorization code for an API key..."
        ) {
            host.event(ProviderEvent::Info {
                message: line.into(),
            })?;
        }
        Ok(())
    }
    fn publish_url(
        &mut self,
        host: &mut impl ProviderInteraction,
        instructions: Option<&str>,
    ) -> Result<()> {
        if let Some(url) = self.pending_url.take() {
            host.event(ProviderEvent::AuthUrl {
                url,
                instructions: instructions.map(str::to_owned),
            })?;
        }
        Ok(())
    }
    fn prompt(&mut self) -> Result<Option<Prompt>> {
        let Ok(line) = std::str::from_utf8(&self.line) else {
            return Ok(None);
        };
        let Some(msg) = line.strip_suffix(": ") else {
            return Ok(None);
        };
        let prompt = if self.method.method == Method::Oauth {
            if msg.starts_with("Complete login in your browser, or paste ")
                || msg.starts_with("Complete sign-in in your browser, or paste ")
                || msg.starts_with("Paste the code Anthropic shows after you sign in:")
            {
                // All Pi 1.6 manual-code prompts include a parenthesized
                // placeholder. Wait for its closing delimiter across PTY reads.
                let Some((message, placeholder)) =
                    msg.strip_suffix(')').and_then(|s| s.rsplit_once(" ("))
                else {
                    return Ok(None);
                };
                Prompt::ManualCode {
                    message: message.into(),
                    placeholder: Some(placeholder.into()),
                }
            } else if msg.starts_with("GitHub Enterprise URL/domain") {
                Prompt::Text {
                    message: "GitHub Enterprise URL/domain (blank for github.com)".into(),
                    placeholder: Some("company.ghe.com".into()),
                }
            } else {
                return Ok(None);
            }
        } else if msg.starts_with("Select 1-") {
            let choices: &[(&str, &str)] = match self.id {
                "amazon-bedrock" => &[
                    ("bearer-token", "Bearer token"),
                    ("aws-profile", "AWS profile"),
                    ("credential-chain", "Existing AWS credential chain"),
                ],
                "google-vertex" => &[
                    ("api-key", "Google Cloud API key"),
                    ("adc", "Application Default Credentials"),
                    ("service-account", "Service account credentials file"),
                ],
                _ => return Err(Kind::InteractionFailed.error("Unrecognized Pi choice prompt")),
            };
            if msg != format!("Select 1-{}", choices.len())
                || !choices
                    .iter()
                    .map(|c| c.1)
                    .eq(self.options.iter().map(String::as_str))
            {
                return Err(
                    Kind::InteractionFailed.error("Pi changed the available provider options")
                );
            }
            self.options.clear();
            let name = if self.id == "amazon-bedrock" {
                "Amazon Bedrock"
            } else {
                "Google Vertex AI"
            };
            Prompt::Select {
                message: format!("Select {name} authentication method:"),
                options: choices
                    .iter()
                    .map(|(id, label)| Choice {
                        id: (*id).into(),
                        label: (*label).into(),
                        description: None,
                    })
                    .collect(),
            }
        } else if msg == format!("Enter {}", self.method.name)
            || matches!(
                msg,
                "Enter Amazon Bedrock bearer token" | "Enter Google Cloud API key"
            )
        {
            Prompt::Secret {
                placeholder: None,
                message: msg.into(),
            }
        } else if matches!(
            msg,
            "Enter AWS profile name"
                | "Configure AWS credentials, then press Enter to continue"
                | "Enter Google Cloud project ID"
                | "Enter Google Cloud location"
                | "Enter service account credentials file path"
                | "Enter Cloudflare account ID"
                | "Enter Cloudflare AI Gateway ID"
        ) {
            Prompt::Text {
                message: msg.into(),
                placeholder: None,
            }
        } else {
            return Ok(None);
        };
        Ok(Some(prompt))
    }
}
fn http_url(s: &str) -> Result<Url> {
    Url::parse(s)
        .ok()
        .filter(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.has_host()
                && u.username().is_empty()
                && u.password().is_none()
        })
        .ok_or_else(|| Error::Protocol("Invalid Pi sign-in URL".into()))
}
