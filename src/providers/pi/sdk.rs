//! A private pipe to one operation of Pi's ModelRuntime. Only public metadata,
//! interaction frames and opaque prompt IDs travel from the SDK to the host.
use super::PiProviders;
use crate::{
    Error, Result,
    account::LoginCancellation,
    process::OwnedProcess,
    providers::{
        Choice, LoginMethod, LoginProvider, Method, Prompt, ProviderErrorKind as Kind,
        ProviderEvent, ProviderInteraction,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::VecDeque, fs, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{ChildStdin, ChildStdout},
};

/// Optional host identity used by Pi's OAuth flow. The device callback runs
/// only if the selected provider asks for an installation UUID.
#[derive(Default)]
pub struct PiLoginOptions<'a> {
    pub agent_name: Option<&'a str>,
    pub get_device_id: Option<&'a (dyn Fn() -> Result<uuid::Uuid> + Send + Sync)>,
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
const PLUGIN: Option<&[u8]> = Some(include_bytes!(concat!(env!("OUT_DIR"), "/pi-auth.node")));
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const PLUGIN: Option<&[u8]> = None;

fn protocol() -> Error {
    Kind::ServerFailed.error("Invalid response from Pi's provider SDK")
}
fn interaction() -> Error {
    Kind::InteractionFailed.error("Pi provider interaction failed")
}

pub(super) struct Host {
    child: OwnedProcess,
    _files: tempfile::TempDir,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
}
impl Host {
    pub async fn start(pi: &PiProviders) -> Result<Option<Self>> {
        use crate::pi_sdk::{self, CompanionError};
        let (dist, paired) = if let Some(dist) = pi_sdk::sdk(&pi.command) {
            (dist, false)
        } else if let Some(dir) = pi.probe.get("KAR_PI_SDK_DIR").filter(|p| !p.is_empty()) {
            let companion = pi_sdk::companion(&pi.probe.cwd.join(dir), pi.version.as_deref())
                .map_err(|error| {
                    let kind = match error {
                        CompanionError::Installation => Kind::UnsupportedInstallation,
                        CompanionError::Version { .. } => Kind::VersionUnsupported,
                    };
                    kind.error(error.message())
                })?;
            (companion.dist, true)
        } else {
            return Ok(None);
        };
        let unavailable = || {
            if paired {
                Err(Kind::UnsupportedInstallation
                    .error("The configured Pi SDK requires a Node host with native addon support"))
            } else {
                Ok(None)
            }
        };
        let Some(bytes) = PLUGIN else {
            return unavailable();
        };
        let mut builder = tempfile::Builder::new();
        builder.prefix("kar-pi-auth-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let files = builder.tempdir()?;
        let library = files.path().canonicalize()?.join("pi-auth.node");
        fs::write(&library, bytes)?;
        let mut command = pi.probe.command(&pi_sdk::node_for(&pi.command));
        command
            .arg("--require")
            .arg(&library)
            .args(["-e", ""])
            .env("KAR_RS_PI_AUTH_SDK", dist.join("index.js"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = match OwnedProcess::spawn(command) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return unavailable(),
            Err(e) => return Err(e.into()),
        };
        let input = child.inner().stdin.take().expect("piped stdin");
        let output = BufReader::new(child.inner().stdout.take().expect("piped stdout")).lines();
        let mut host = Self {
            child,
            _files: files,
            input,
            output,
        };
        // Before READY the SDK has received no operation and cannot have
        // changed credentials. Preserve the terminal adapter for other installs.
        match tokio::time::timeout(pi.timeout, host.output.next_line()).await {
            Ok(Ok(Some(line))) if line == "KAR-AUTH READY " => Ok(Some(host)),
            _ => {
                host.child.stop().await;
                unavailable()
            }
        }
    }
    pub(super) async fn send(&mut self, v: &impl Serialize) -> Result<()> {
        let mut bytes = serde_json::to_vec(v)?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await?;
        Ok(())
    }
    pub(super) async fn next(&mut self) -> Result<(String, String)> {
        let frame = self.output.next_line().await?.ok_or_else(protocol)?;
        let (tag, data) = frame
            .strip_prefix("KAR-AUTH ")
            .and_then(|s| s.split_once(' '))
            .ok_or_else(protocol)?;
        if tag == "ERROR" {
            return Err(match data {
                "unknown" => Kind::UnknownProvider.error("Unknown Pi provider"),
                "method" => Kind::MethodUnavailable.error("Pi does not offer this login method"),
                "rejected" => Kind::Rejected.error("Pi did not complete the provider operation"),
                _ => protocol(),
            });
        }
        Ok((tag.into(), data.into()))
    }
    pub(super) async fn picker(&mut self) -> Result<String> {
        // Project extensions can log on stdout while registering their providers.
        while let Some(line) = self.output.next_line().await? {
            if let Some(data) = line.strip_prefix("KAR-AUTH PICKER ") {
                return Ok(data.into());
            }
            if line.starts_with("KAR-AUTH ERROR ") {
                return Err(protocol());
            }
        }
        Err(protocol())
    }
    pub(super) fn config(pi: &PiProviders, op: &str) -> Result<Value> {
        Ok(json!({
            "op": op,
            "agentDir": serde_json::to_value(&pi.agent_dir)?,
            "authPath": serde_json::to_value(&pi.auth_path)?,
            "modelsPath": serde_json::to_value(&pi.models_path)?,
            "allowModelNetwork": false,
            "refreshOnCreate": false,
        }))
    }
    pub(super) async fn close(mut self) {
        drop(self.input);
        let _ = tokio::time::timeout(Duration::from_millis(800), self.child.wait()).await;
    }
    pub async fn status(
        mut self,
        pi: &PiProviders,
        id: &str,
    ) -> Result<crate::providers::ProviderStatus> {
        let mut config = Self::config(pi, "status")?;
        config["provider"] = json!(id);
        self.send(&config).await?;
        let (tag, data) = self.next().await?;
        let status: crate::providers::ProviderStatus =
            serde_json::from_str(&data).map_err(|_| protocol())?;
        if tag != "STATUS" || status.provider_id != id {
            return Err(protocol());
        }
        Ok(status)
    }
    pub async fn logout(mut self, pi: &PiProviders, id: &str) -> Result<()> {
        let mut config = Self::config(pi, "logout")?;
        config["provider"] = json!(id);
        self.send(&config).await?;
        if self.next().await?.0 != "OK" {
            return Err(protocol());
        }
        Ok(())
    }
    pub async fn menu(mut self, pi: &PiProviders) -> Result<Vec<LoginProvider>> {
        self.send(&Self::config(pi, "menu")?).await?;
        let (tag, data) = self.next().await?;
        if tag != "MENU" {
            return Err(protocol());
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Entry {
            name: String,
            #[serde(default)]
            ambient: bool,
            #[serde(default)]
            is_subscription: bool,
            login_label: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Row {
            id: String,
            name: String,
            oauth: Option<Entry>,
            api_key: Option<Entry>,
        }
        let rows: Vec<Row> = serde_json::from_str(&data).map_err(|_| protocol())?;
        let mut providers = Vec::new();
        for row in rows {
            let mut methods = vec![];
            if let Some(m) = row.oauth {
                methods.push(LoginMethod {
                    method: Method::Oauth,
                    name: m.name,
                    subscription: Some(m.is_subscription),
                    ambient: false,
                    login_label: m.login_label,
                });
            }
            if let Some(m) = row.api_key {
                methods.push(LoginMethod {
                    method: Method::ApiKey,
                    name: m.name,
                    subscription: None,
                    ambient: m.ambient,
                    login_label: None,
                });
            }
            if !methods.is_empty() {
                providers.push(LoginProvider {
                    provider_id: row.id,
                    name: row.name,
                    methods,
                });
            }
        }
        let order = icu_collator::Collator::try_new(Default::default(), Default::default())
            .map_err(|_| protocol())?;
        providers.sort_by(|a, b| order.compare(&a.name, &b.name));
        Ok(providers)
    }
    async fn device(&mut self, options: &PiLoginOptions<'_>) -> Result<()> {
        let id = options.get_device_id.ok_or_else(interaction)?().map_err(|_| interaction())?;
        self.input.write_all(format!("D{id}\n").as_bytes()).await?;
        Ok(())
    }
    pub async fn login(
        mut self,
        pi: &PiProviders,
        id: &str,
        method: Method,
        host: &mut impl ProviderInteraction,
        options: &PiLoginOptions<'_>,
        cancel: &LoginCancellation,
    ) -> Result<()> {
        let mut cfg = Self::config(pi, "login")?;
        cfg["provider"] = json!(id);
        cfg["method"] = json!(method);
        cfg["hostDevice"] = json!(options.get_device_id.is_some());
        if let Some(name) = options.agent_name {
            cfg["agentName"] = json!(name);
        }
        self.send(&cfg).await?;
        let result = tokio::select! {biased;
            _=cancel.cancelled()=>Err(crate::providers::cancelled()),
            r=tokio::time::timeout(pi.login_timeout,self.converse(method,host,options))=>
                r.unwrap_or_else(|_|Err(Kind::TimedOut.error("Pi provider login timed out"))),
        };
        // EOF releases the input thread even if Pi is waiting for a host UUID.
        // Allow Pi to close callback sockets and finish writing credentials.
        if result.is_err() {
            let _ = tokio::time::timeout(
                Duration::from_millis(200),
                self.send(&json!({"cancel":true})),
            )
            .await;
        }
        self.close().await;
        result
    }
    async fn converse(
        &mut self,
        method: Method,
        host: &mut impl ProviderInteraction,
        options: &PiLoginOptions<'_>,
    ) -> Result<()> {
        let mut deferred = VecDeque::new();
        loop {
            let (tag, data) = match deferred.pop_front() {
                Some(f) => f,
                None => self.next().await?,
            };
            match tag.as_str() {
                "OK" => return Ok(()),
                "EVENT" => host.event(event(&data)?).map_err(|_| interaction())?,
                "DEVICE" => self.device(options).await?,
                "DISMISS" => {}
                "PROMPT" => {
                    let (id, prompt) = prompt(&data)?;
                    let is_key =
                        method == Method::ApiKey && matches!(prompt, Prompt::Secret { .. });
                    let choices = match &prompt {
                        Prompt::Select { options, .. } => {
                            Some(options.iter().map(|c| c.id.clone()).collect::<Vec<_>>())
                        }
                        _ => None,
                    };
                    let mut live_events = host.concurrent_events();
                    let answer = host.prompt(prompt);
                    tokio::pin!(answer);
                    loop {
                        tokio::select! {biased;
                            incoming=self.next()=>{
                                let (tag,data)=incoming?;
                                match tag.as_str() {
                                    "DEVICE"=>self.device(options).await?,
                                    "DISMISS" if serde_json::from_str::<String>(&data).ok().as_deref()==Some(&id)=>break,
                                    "DISMISS"=>{
                                        let dismissed:String=serde_json::from_str(&data).map_err(|_|protocol())?;
                                        deferred.retain(|(t,v)| t!="PROMPT" || prompt_id(v).as_deref()!=Some(&dismissed));
                                    }
                                    "EVENT" if live_events.is_some()=>live_events.as_mut().unwrap()(event(&data)?).map_err(|_|interaction())?,
                                    "OK"=>{deferred.push_back((tag,data));break;}
                                    "PROMPT"|"EVENT"=>deferred.push_back((tag,data)),
                                    _=>return Err(protocol())
                                }
                            }
                            result=&mut answer=>{
                                let mut value=result.map_err(|_|interaction())?;
                                if choices.as_ref().is_some_and(|list| !list.contains(&value)){return Err(interaction())}
                                if is_key{value=crate::pi_sdk::config_literal(&value);}
                                self.send(&json!({"id":id,"answer":value})).await?;break;
                            }
                        }
                    }
                }
                _ => return Err(protocol()),
            }
        }
    }
}
fn prompt_id(data: &str) -> Option<String> {
    serde_json::from_str::<Value>(data).ok()?["id"]
        .as_str()
        .map(String::from)
}
fn prompt(data: &str) -> Result<(String, Prompt)> {
    #[derive(Deserialize)]
    struct Wire {
        id: String,
        #[serde(rename = "type")]
        kind: String,
        message: String,
        placeholder: Option<String>,
        options: Option<Vec<Choice>>,
    }
    let p: Wire = serde_json::from_str(data).map_err(|_| protocol())?;
    let prompt = match p.kind.as_str() {
        "text" => Prompt::Text {
            message: p.message,
            placeholder: p.placeholder,
        },
        "secret" => Prompt::Secret {
            message: p.message,
            placeholder: p.placeholder,
        },
        "manual_code" => Prompt::ManualCode {
            message: p.message,
            placeholder: p.placeholder,
        },
        "select" => Prompt::Select {
            message: p.message,
            options: p.options.ok_or_else(protocol)?,
        },
        _ => return Err(protocol()),
    };
    Ok((p.id, prompt))
}
fn event(data: &str) -> Result<ProviderEvent> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Wire {
        #[serde(rename = "type")]
        kind: String,
        url: Option<String>,
        instructions: Option<String>,
        user_code: Option<String>,
        verification_uri: Option<String>,
        interval_seconds: Option<u64>,
        expires_in_seconds: Option<u64>,
        message: Option<String>,
    }
    let e: Wire = serde_json::from_str(data).map_err(|_| protocol())?;
    Ok(match e.kind.as_str() {
        "auth_url" => ProviderEvent::AuthUrl {
            url: e.url.ok_or_else(protocol)?,
            instructions: e.instructions,
        },
        "device_code" => ProviderEvent::DeviceCode {
            user_code: e.user_code.ok_or_else(protocol)?,
            verification_uri: e.verification_uri.ok_or_else(protocol)?,
            interval_seconds: e.interval_seconds,
            expires_in_seconds: e.expires_in_seconds,
        },
        "info" | "progress" => ProviderEvent::Info {
            message: e.message.ok_or_else(protocol)?,
        },
        _ => return Err(protocol()),
    })
}
