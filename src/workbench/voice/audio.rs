//! Little-endian bounded 16 kHz mono PCM and recording preferences.
use super::*;
use std::collections::HashSet;
pub(super) const OPTIONS_MAX: usize = 128000;
pub(super) const CHUNK: usize = 64000;
pub(super) const PCM_MAX: usize = CHUNK * 300;
fn invalid() -> Error {
    Error::Refused("invalid_audio".into())
}
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Word {
    pub term: String,
    pub aliases: Vec<String>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Options {
    pub speech_language: String,
    pub speech_translate_to_english: bool,
    pub speech_custom_words: Vec<Word>,
    #[serde(default)]
    pub speech_hints: Vec<String>,
    pub speech_remove_filler_words: bool,
    #[serde(default)]
    pub speech_correction_word: Option<String>,
    #[serde(default = "yes")]
    pub speech_silence_filtering: bool,
}
fn yes() -> bool {
    true
}
fn units(s: &str) -> usize {
    s.encode_utf16().count()
}
pub(super) fn words(list: &[Word]) -> Result<()> {
    let mut seen = HashSet::new();
    if list.len() > 100 {
        return Err(invalid());
    }
    for entry in list {
        if entry.aliases.len() > 8 {
            return Err(invalid());
        }
        for word in std::iter::once(&entry.term).chain(&entry.aliases) {
            let key = word.trim().to_lowercase();
            if key.is_empty() || units(word) > 50 || !seen.insert(key) {
                return Err(invalid());
            }
        }
    }
    Ok(())
}
impl Options {
    pub fn validate(&self) -> Result<()> {
        if !(2..=16).contains(&units(&self.speech_language))
            || self.speech_hints.len() > 8
            || self
                .speech_hints
                .iter()
                .any(|s| !(1..=50).contains(&units(s)))
            || self
                .speech_correction_word
                .as_deref()
                .is_some_and(|s| units(s) > 50)
        {
            return Err(invalid());
        }
        words(&self.speech_custom_words)
    }
    pub fn recognition_words(&self) -> Vec<&str> {
        let mut seen = HashSet::new();
        self.speech_hints
            .iter()
            .chain(self.speech_custom_words.iter().map(|w| &w.term))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && seen.insert(s.to_lowercase()))
            .collect()
    }
    pub fn corrected(&self, s: &str, language: &str) -> String {
        let s = aliases(s, &self.speech_custom_words);
        if !self.speech_remove_filler_words {
            return s.trim().into();
        }
        let mut list = vec![
            "uh", "uhm", "umm", "uhh", "uhhh", "ehh", "ehm", "ahm", "hmm", "hm", "mmm", "хм", "ммм",
        ];
        match language.split(['-', '_']).next() {
            Some("en") => list.extend(["um", "ah", "eh"]),
            Some("de") => list.extend(["äh", "ähm"]),
            Some("fr") => list.push("euh"),
            _ => {}
        }
        let mut parts = vec![];
        if let Some(cue) = self
            .speech_correction_word
            .as_ref()
            .filter(|s| !s.trim().is_empty())
        {
            parts.push(format!("(?P<cue>{})", phrase(cue)));
        }
        parts.push(format!(
            "(?:{})(?:[,.])?",
            list.into_iter()
                .map(|s| format!(r"{}\b", regex::escape(s)))
                .collect::<Vec<_>>()
                .join("|")
        ));
        let pattern =
            regex::Regex::new(&format!(r"(?iu)\b(?:{})", parts.join("|"))).expect("escaped words");
        static SPACE: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(r"[^\S\r\n]{2,}").unwrap());
        static LINES: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(r"[^\S\r\n]*([\r\n])[^\S\r\n]*").unwrap());
        let corrected = pattern.replace_all(&s, |c: &regex::Captures| {
            c.name("cue").map_or("", |m| m.as_str()).to_owned()
        });
        LINES
            .replace_all(&SPACE.replace_all(&corrected, " "), "$1")
            .trim()
            .into()
    }
}
fn phrase(s: &str) -> String {
    s.split_whitespace()
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(r"\s+")
}
pub(super) fn aliases(s: &str, words: &[Word]) -> String {
    let mut alts: Vec<_> = words
        .iter()
        .flat_map(|w| w.aliases.iter().map(|a| (a, &w.term)))
        .filter(|(a, _)| !a.trim().is_empty())
        .collect();
    alts.sort_by_key(|(a, _)| std::cmp::Reverse(a.len()));
    if alts.is_empty() {
        return s.into();
    }
    let pattern = alts
        .iter()
        .enumerate()
        .map(|(i, (a, _))| format!("(?P<a{i}>{})", phrase(a)))
        .collect::<Vec<_>>()
        .join("|");
    let Ok(re) = regex::Regex::new(&format!(r"(?iu)(?:{pattern})")) else {
        return s.into();
    };
    let word = |c: char| c.is_alphanumeric() || c == '_';
    re.replace_all(s, |c: &regex::Captures| {
        let m = c.get(0).unwrap();
        if s[..m.start()].chars().next_back().is_some_and(word)
            || s[m.end()..].chars().next().is_some_and(word)
        {
            return m.as_str().to_owned();
        }
        alts.iter()
            .enumerate()
            .find_map(|(i, (_, w))| c.name(&format!("a{i}")).map(|_| w.as_str()))
            .unwrap_or(m.as_str())
            .to_owned()
    })
    .into_owned()
}
pub(super) fn split(body: &[u8]) -> Result<(Options, &[u8])> {
    let n = u32::from_le_bytes(
        body.get(..4)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(invalid)?,
    ) as usize;
    if n > OPTIONS_MAX || 4 + n >= body.len() {
        return Err(invalid());
    }
    let opts: Options = serde_json::from_slice(&body[4..4 + n]).map_err(|_| invalid())?;
    opts.validate()?;
    let data = &body[4 + n..];
    validate(data)?;
    Ok((opts, data))
}
pub(super) fn validate(data: &[u8]) -> Result<()> {
    if data.is_empty() || data.len() > PCM_MAX || data.len() % 4 != 0 {
        return Err(invalid());
    }
    if data
        .chunks_exact(4)
        .any(|v| !(-1.0..=1.0).contains(&f32::from_le_bytes(v.try_into().unwrap())))
    {
        return Err(invalid());
    }
    Ok(())
}
pub(super) fn wave(data: &[u8]) -> Vec<u8> {
    let bytes = (data.len() / 2) as u32;
    let mut out = Vec::with_capacity(44 + bytes as usize);
    out.extend(b"RIFF");
    out.extend((36 + bytes).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(16000u32.to_le_bytes());
    out.extend(32000u32.to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(bytes.to_le_bytes());
    for raw in data.chunks_exact(4) {
        let v = f32::from_le_bytes(raw.try_into().unwrap()) as f64;
        let n = (v * if v < 0.0 { 32768.0 } else { 32767.0 } + 0.5).floor() as i16;
        out.extend(n.to_le_bytes());
    }
    out
}
