//! Native inference runs in the host-installed transcribe-cli process.
use super::*;
use std::{process::Stdio, time::Duration};
use tokio::process::Command;
pub(super) struct Devices {
    pub program: PathBuf,
    pub gpu: Vec<Value>,
}
fn binary() -> Option<PathBuf> {
    let cmd = std::env::var_os("KAR_VOICE_ENGINE").unwrap_or_else(|| "transcribe-cli".into());
    let path = Path::new(&cmd);
    let p = if path.is_absolute() {
        path.to_owned()
    } else {
        if path.components().count() != 1 {
            return None;
        }
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(&cmd))
            .find(|p| p.is_file())?
    };
    p.canonicalize().ok().filter(|p| p.is_file())
}
impl Devices {
    async fn discover() -> Result<Self> {
        let program = binary().ok_or_else(|| {
            Error::Unsupported(
                "Install transcribe-cli 0.4.1 or set KAR_VOICE_ENGINE to the native executable"
                    .into(),
            )
        })?;
        let mut cmd = Command::new(&program);
        cmd.arg("--list-devices")
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .env_remove("KAR_WORKBENCH_TOKEN");
        let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
            .await
            .map_err(|_| Error::Timeout("discovering speech devices".into()))??;
        if !out.status.success() {
            return Err(Error::Unsupported(
                "native speech device discovery failed".into(),
            ));
        }
        let mut current = None;
        let mut gpu = vec![];
        let mut any = false;
        for line in String::from_utf8_lossy(&out.stdout).lines().map(str::trim) {
            if let Some((idx, name)) = line.strip_prefix('[').and_then(|s| s.split_once("] ")) {
                current = idx.parse::<usize>().ok().map(|i| (i, name.to_owned()));
                continue;
            }
            if let Some((i, name)) = current.take() {
                let device_type = line
                    .split_whitespace()
                    .find_map(|f| f.strip_prefix("type="));
                any |= device_type.is_some();
                if matches!(device_type, Some("gpu" | "igpu")) {
                    gpu.push(json!({"id":i.to_string(),"name":name}));
                }
            }
        }
        if !any {
            return Err(Error::Unsupported(
                "no usable native speech compute device".into(),
            ));
        }
        Ok(Self { program, gpu })
    }
}
impl Service {
    pub(super) async fn devices(&self) -> Result<&Devices> {
        self.engine.get_or_try_init(Devices::discover).await
    }
    pub(super) fn status(&self, gpu: &[Value]) -> Result<Value> {
        let cfg = self.config();
        let m = model(&cfg.model)?;
        let state = if !self.ready(m, &self.path(m, &cfg)) {
            "missing-model"
        } else if self.operation.available_permits() == 0 {
            "transcribing"
        } else {
            "ready"
        };
        Ok(
            json!({"supported":true,"state":state,"modelId":m.id,"model":m.details["name"],"size":m.size,
            "supportsStreaming":false,"supportsBufferedStreaming":true,"supportsModelLinking":true,"supportsTranslation":m.details["supportsTranslation"],
            "languages":m.details["languages"],"supportsLanguageDetection":m.details["supportsLanguageDetection"],"acceleration":cfg.acceleration,
            "benchmarkTier":if cfg.acceleration=="cpu"||gpu.is_empty(){"cpu"}else{"accelerated"},
            "modelUnloadTimeout":"immediately","gpuDevices":gpu}),
        )
    }
}

impl Service {
    pub(super) async fn recognize(
        &self,
        m: &Model,
        path: &Path,
        cfg: &Settings,
        opts: &super::audio::Options,
        data: &[u8],
    ) -> Result<String> {
        let engine = self.devices().await?;
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("audio.wav");
        let result = dir.path().join("transcript.txt");
        tokio::fs::write(&input, super::audio::wave(data)).await?;
        let mut cmd = Command::new(&engine.program);
        cmd.kill_on_drop(true)
            .current_dir(dir.path())
            .env_remove("KAR_WORKBENCH_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .args(["--quiet", "--model"])
            .arg(path)
            .arg("--output")
            .arg(&result);
        match cfg.acceleration.as_str() {
            "cpu" => {
                cmd.args(["--backend", "cpu"]);
            }
            "auto" => {}
            gpu => {
                let id = gpu.strip_prefix("gpu:").unwrap_or("");
                if !engine.gpu.iter().any(|g| g["id"] == id) {
                    return Err(Error::Unsupported(
                        "selected speech accelerator unavailable".into(),
                    ));
                }
                cmd.args(["--device", id]);
            }
        }
        let languages = m.details["languages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        let language = if languages.len() == 1 {
            languages[0]
        } else if languages.contains(&opts.speech_language.as_str()) {
            opts.speech_language.as_str()
        } else if m.details["supportsLanguageDetection"] == true {
            "auto"
        } else if languages.contains(&"en") {
            "en"
        } else {
            languages.first().copied().unwrap_or("auto")
        };
        if language != "auto" {
            cmd.args(["--language", language]);
        }
        let translate = opts.speech_translate_to_english
            && m.details["supportsTranslation"] == true
            && language != "en";
        if translate {
            cmd.args(["--task", "translate"]);
        }
        let terms = opts
            .recognition_words()
            .into_iter()
            .filter(|s| !s.contains(['\n', '\r']))
            .collect::<Vec<_>>();
        if !translate && !terms.is_empty() {
            let vocab = dir.path().join("vocabulary.txt");
            tokio::fs::write(&vocab, terms.join("\n")).await?;
            cmd.arg("--vocabulary-file").arg(vocab);
        }
        let code = tokio::time::timeout(Duration::from_secs(300), cmd.arg(&input).status())
            .await
            .map_err(|_| Error::Timeout("transcribing speech".into()))??;
        if !code.success() {
            return Err(Error::Refused("native speech inference failed".into()));
        }
        let mut text = String::new();
        use tokio::io::AsyncReadExt;
        let file = tokio::fs::File::open(result).await?;
        file.take(1_000_001).read_to_string(&mut text).await?;
        if text.len() > 1_000_000 {
            return Err(Error::Refused(
                "native speech transcript exceeds limit".into(),
            ));
        }
        Ok(opts.corrected(&text, if translate { "en" } else { language }))
    }
}

impl Service {
    /// The native CLI emits this header only after both model load and ASR session initialization.
    pub(super) async fn check_load(&self, _m: &Model, path: &Path, cfg: &Settings) -> Result<()> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
        let engine = self.devices().await?;
        let dir = tempfile::tempdir()?;
        let audio = dir.path().join("silence.wav");
        tokio::fs::write(&audio, super::audio::wave(&[0; 640])).await?;
        let batch = dir.path().join("files.list");
        tokio::fs::write(&batch, format!("{}\n", audio.display())).await?;
        let mut cmd = Command::new(&engine.program);
        cmd.kill_on_drop(true)
            .env_remove("KAR_WORKBENCH_TOKEN")
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .args(["--quiet", "--model"])
            .arg(path)
            .arg("--batch")
            .arg(batch)
            .arg("--batch-jsonl");
        match cfg.acceleration.as_str() {
            "cpu" => {
                cmd.args(["--backend", "cpu"]);
            }
            "auto" => {}
            v => {
                let id = v.strip_prefix("gpu:").unwrap_or("");
                if !engine.gpu.iter().any(|d| d["id"] == id) {
                    return Err(Error::Unsupported(
                        "selected speech accelerator unavailable".into(),
                    ));
                }
                cmd.args(["--device", id]);
            }
        }
        let mut child = cmd.spawn()?;
        let mut line = String::new();
        let stdout = child.stdout.take().unwrap();
        tokio::time::timeout(
            Duration::from_secs(120),
            BufReader::new(stdout.take(8192)).read_line(&mut line),
        )
        .await
        .map_err(|_| Error::Timeout("preparing speech model".into()))??;
        let _ = child.kill().await;
        if serde_json::from_str::<Value>(&line)
            .ok()
            .is_some_and(|v| v["type"] == "batch_header")
        {
            Ok(())
        } else {
            Err(Error::Refused(
                "native speech engine could not prepare selected ASR model".into(),
            ))
        }
    }
}
