//! From the Kar repository root, see docs/backend-coverage.md.
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::Path,
    process::{Command, ExitCode},
};

type Row = Vec<String>;

// These versioned tables contain one logical row per line. Evidence cells may
// use CSV quoting for literal quotes or tabs (in particular, CLI commands).
fn cells(line: &str) -> Result<Row, String> {
    let mut row = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        let mut cell = String::new();
        if chars.next_if_eq(&'"').is_some() {
            loop {
                match chars.next() {
                    Some('"') if chars.next_if_eq(&'"').is_some() => cell.push('"'),
                    Some('"') => break,
                    Some(c) => cell.push(c),
                    None => return Err("unterminated quoted cell".into()),
                }
            }
        } else {
            while chars.peek().is_some_and(|c| *c != '\t') {
                cell.push(chars.next().unwrap());
            }
        }
        row.push(cell);
        match chars.next() {
            None => return Ok(row),
            Some('\t') => {}
            _ => return Err("trailing data after quoted cell".into()),
        }
    }
}

fn read(path: &str, header: &str) -> Result<Vec<Row>, String> {
    let input = fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut lines = input.lines();
    if lines.next() != Some(header) {
        return Err(format!("{path}: unexpected header"));
    }
    let width = header.split('\t').count();
    lines
        .enumerate()
        .map(|(i, line)| {
            let row = cells(line).map_err(|e| format!("{path}:{}: {e}", i + 2))?;
            if row.len() != width {
                return Err(format!("{path}:{}: expected {width} columns", i + 2));
            }
            Ok(row)
        })
        .collect()
}

// Scope an assertion to one Rust function when other scenarios reuse the same check.
fn anchor_line(source: &str, anchor: &str) -> Option<usize> {
    if let Some((scope, needle)) = anchor.split_once(" >> ") {
        let start = source.lines().position(|line| line.contains(scope))?;
        return source
            .lines()
            .enumerate()
            .skip(start)
            .take_while(|(_, line)| *line != "}")
            .find_map(|(i, line)| line.contains(needle).then_some(i + 1));
    }
    source
        .lines()
        .position(|line| line.contains(anchor))
        .map(|i| i + 1)
}

fn git_file(repo: &Path, commit: &str, file: &str) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["show", &format!("{commit}:{file}")])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("missing {file} at {commit}"));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

/// Read the pinned Git object, including when the checkout has moved on.
fn check_sources(repo: &Path, index: &[Row]) -> Result<(), String> {
    const PIN: &str = "2b15859a13dcc50e8b66cbaee5a007832aa00e08";
    let baseline = fs::read_to_string("docs/backend-acceptance.toml").map_err(|e| e.to_string())?;
    let runtime = baseline
        .split_once("[baseline.runtime]")
        .and_then(|(_, s)| s.split_once("\n["))
        .map(|(s, _)| s)
        .unwrap_or_default();
    if !runtime
        .lines()
        .any(|line| line.trim() == format!("commit = \"{PIN}\""))
    {
        return Err("audit baseline differs from docs/backend-acceptance.toml".into());
    }
    let mut files = BTreeMap::new();
    for row in index {
        let (path, start) = row[1]
            .rsplit_once("#L")
            .ok_or_else(|| format!("invalid source: {}", row[1]))?;
        if !files.contains_key(path) {
            files.insert(
                path,
                git_file(repo, PIN, path)?
                    .lines()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            );
        }
        for line in std::iter::once(start).chain(row[3].split(',').filter(|s| !s.is_empty())) {
            let number = line
                .parse::<usize>()
                .map_err(|_| format!("invalid line {line} in {}", row[1]))?;
            let content = number
                .checked_sub(1)
                .and_then(|i| files[path].get(i))
                .ok_or_else(|| format!("invalid {path} line {line}"))?;
            if (row[0] == "public-case" || row[0].starts_with("cli-"))
                && line == start
                && !content.contains(&row[2])
            {
                return Err(format!("source locator {} missing at {}", row[2], row[1]));
            }
        }
    }
    eprintln!("{} upstream files checked at {PIN}", files.len());
    Ok(())
}

fn print_row(fields: &[&str]) {
    let cells: Vec<_> = fields
        .iter()
        .map(|s| {
            if s.contains(['\t', '\n', '"']) {
                format!("\"{}\"", s.replace('"', "\"\""))
            } else {
                s.to_string()
            }
        })
        .collect();
    println!("{}", cells.join("\t"));
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let mut filter = String::new();
    let mut upstream = None;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--filter" => filter = value,
            "--upstream" => upstream = Some(value),
            _ => return Err(format!("unknown flag {flag}; see docs/backend-coverage.md")),
        }
    }
    let index = read(
        "docs/backend-source-index.tsv",
        "kind\tupstream\tlocator\tdirect_assertion_lines\tevidence",
    )?;
    let evidence = read(
        "docs/backend-evidence.tsv",
        "upstream\tarea\truntime\tversion\ttransport\tplatform\toperation\texpected\tkar_commit\tcommand\tobserved\tstatus",
    )?;
    let links = read(
        "docs/backend-assertion-map.tsv",
        "source\tlines\tcommit\truntimes\tevidence_kind\tkar_file\tanchor\tnote",
    )?;
    let cli_rows = read(
        "docs/backend-cli-map.tsv",
        "kind	upstream	locator	commit	kar_file	anchor	scope	command	operation	expected",
    )?;
    let mut cli = BTreeMap::new();
    let mut cli_files = BTreeMap::new();
    for row in &cli_rows {
        let key = (&row[0], &row[1], &row[2]);
        let text = cli_files
            .entry((&row[3], &row[4]))
            .or_insert_with(|| git_file(Path::new("."), &row[3], &row[4]))
            .as_ref()
            .map_err(Clone::clone)?;
        let line = anchor_line(text, &row[5])
            .ok_or_else(|| format!("missing CLI anchor in {}:{}: {}", row[3], row[4], row[5]))?;
        if cli.insert(key, (row, line)).is_some() {
            return Err(format!("duplicate CLI locator {key:?}"));
        }
    }
    let indexed: BTreeSet<_> = index
        .iter()
        .filter(|row| row[0].starts_with("cli-"))
        .map(|row| (&row[0], &row[1], &row[2]))
        .collect();
    if cli.keys().any(|key| !indexed.contains(key)) || cli.len() != indexed.len() {
        return Err("CLI map and pinned source index contain different locators".into());
    }
    let sdk_rows = read(
        "docs/backend-sdk-map.tsv",
        "kind\tupstream\tlocator\tcommit\tkar_file\tanchor\tlevel\toperation\tcommand\tnote",
    )?;
    let indexed_sdk: BTreeSet<_> = index
        .iter()
        .filter(|r| {
            matches!(
                r[0].as_str(),
                "contract" | "contract-file" | "contract-member" | "package-export"
            )
        })
        .map(|r| (&r[0], &r[1], &r[2]))
        .collect();
    let mut sdk = BTreeMap::new();
    for row in &sdk_rows {
        let key = (&row[0], &row[1], &row[2]);
        if !indexed_sdk.contains(&key) {
            return Err(format!("unknown SDK locator {key:?}"));
        }
        let text = cli_files
            .entry((&row[3], &row[4]))
            .or_insert_with(|| git_file(Path::new("."), &row[3], &row[4]))
            .as_ref()
            .map_err(Clone::clone)?;
        let line = anchor_line(text, &row[5])
            .ok_or_else(|| format!("missing SDK anchor in {}:{}: {}", row[3], row[4], row[5]))?;
        if sdk.insert(key, (row, line)).is_some() {
            return Err(format!("duplicate SDK locator {key:?}"));
        }
    }
    if let Some(repo) = upstream {
        check_sources(Path::new(&repo), &index)?;
    }
    let unique: BTreeSet<_> = evidence.iter().collect();
    let mut joins: BTreeMap<(&str, &str, usize), Vec<(&Row, &Row, usize)>> = BTreeMap::new();
    let mut texts = BTreeMap::new();
    for link in &links {
        let [source, lines, commit, runtimes, _, file, needle, _] = link.as_slice() else {
            unreachable!()
        };
        let case = index
            .iter()
            .find(|r| {
                r[1] == *source
                    && matches!(
                        r[0].as_str(),
                        "public-case" | "vendor-test" | "vendor-shared"
                    )
            })
            .ok_or_else(|| format!("source missing from test index: {source}"))?;
        let snapshot = texts
            .entry((commit, file))
            .or_insert_with(|| git_file(Path::new("."), commit, file))
            .as_ref()
            .map_err(Clone::clone)?;
        let at = anchor_line(snapshot, needle)
            .ok_or_else(|| format!("missing anchor in {commit}:{file}: {needle}"))?;
        let scopes: Vec<_> = unique
            .iter()
            .copied()
            .filter(|e| {
                e[0] == *source && e[8] == *commit && runtimes.split(',').any(|rt| rt == e[2])
            })
            .collect();
        if scopes.is_empty() {
            return Err(format!(
                "no matching evidence for {source} at {commit} ({runtimes})"
            ));
        }
        for token in lines.split(',') {
            let (line, ordinal) = match token.split_once('.') {
                Some((line, n)) => (
                    line,
                    Some(
                        n.parse::<usize>()
                            .map_err(|_| format!("bad assertion ordinal: {token}"))?,
                    ),
                ),
                None => (token, None),
            };
            let count = case[3].split(',').filter(|n| *n == line).count();
            let ordinal = ordinal
                .or((count == 1).then_some(1))
                .ok_or_else(|| format!("specify a unique ordinal for {source}:{token}"))?;
            if !(1..=count).contains(&ordinal) {
                return Err(format!("assertion missing at {source}:{token}"));
            }
            for &scope in &scopes {
                joins
                    .entry((source, line, ordinal))
                    .or_default()
                    .push((link, scope, at));
            }
        }
    }
    print_row(&[
        "kind",
        "source",
        "locator",
        "assertion",
        "level",
        "area",
        "runtime",
        "version",
        "transport",
        "platform",
        "kar_commit",
        "kar_test",
        "command",
        "operation",
        "expected",
        "observed",
        "ledger_status",
        "note",
    ]);
    let mut emitted = false;
    for r in &index {
        if !r[1].contains(&filter) && !r[2].contains(&filter) {
            continue;
        }
        emitted = true;
        if let Some((row, at)) = cli.get(&(&r[0], &r[1], &r[2])) {
            let local = format!("{}#L{at}", row[4]);
            print_row(&[
                &r[0],
                &r[1],
                &r[2],
                "",
                "CLI 实现位置",
                "CLI",
                &row[6],
                "待记录",
                "Kar CLI",
                "macOS",
                &row[3],
                &local,
                &row[7],
                &row[8],
                &row[9],
                "固定上游和 Kar 源码位置已核对，逐命令运行留待统一验收",
                "未完成或失败",
                "环境及版本须随执行结果登记",
            ]);
            continue;
        }
        if let Some((row, at)) = sdk.get(&(&r[0], &r[1], &r[2])) {
            let local = format!("{}#L{at}", row[4]);
            print_row(&[
                &r[0],
                &r[1],
                &r[2],
                "",
                &row[6],
                "SDK",
                "依调用的适配器",
                "待实测记录",
                "Rust API",
                "macOS",
                &row[3],
                &local,
                &row[8],
                &row[7],
                &row[9],
                "固定两端源码已核对；运行留待统一验收",
                "未完成或失败",
                &row[9],
            ]);
            continue;
        }
        let mut occurrences = BTreeMap::new();
        for line in r[3].split(',') {
            let occurrence = occurrences.entry(line).or_insert(0);
            *occurrence += 1;
            let assertion = if line.is_empty() {
                String::new()
            } else {
                format!("{line}.{}", *occurrence)
            };
            let candidates: Vec<_> = unique.iter().copied().filter(|e| e[0] == r[1]).collect();
            let matches = joins.get(&(r[1].as_str(), line, *occurrence));
            for &e in &candidates {
                let verified =
                    matches.and_then(|set| set.iter().find(|(_, scoped, _)| *scoped == e));
                let (label, local, note) = if let Some((link, _, at)) = verified {
                    (
                        link[4].as_str(),
                        format!("{}#L{at}", link[5]),
                        link[7].as_str(),
                    )
                } else {
                    (
                        "同址待核对",
                        String::new(),
                        "同一源码地址有证据，此范围尚未核对到本断言。",
                    )
                };
                print_row(&[
                    &r[0], &r[1], &r[2], &assertion, label, &e[1], &e[2], &e[3], &e[4], &e[5],
                    &e[8], &local, &e[9], &e[6], &e[7], &e[10], &e[11], note,
                ]);
            }
            if candidates.is_empty() {
                print_row(&[
                    &r[0],
                    &r[1],
                    &r[2],
                    &assertion,
                    "待核对",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "",
                    "待建立逐项证据关联",
                ]);
            }
        }
    }
    if !emitted {
        return Err(format!("no indexed source or locator matching {filter:?}"));
    }
    eprintln!(
        "{} source rows, {} evidence rows ({} exact duplicates), {} reviewed assertion mappings, {} CLI mappings, {} SDK mappings",
        index.len(),
        evidence.len(),
        evidence.len() - unique.len(),
        links.len(),
        cli.len(),
        sdk.len()
    );
    Ok(())
}

fn main() -> ExitCode {
    if !Path::new("docs/backend-acceptance.toml").is_file() {
        eprintln!("run from the Kar repository root");
        return ExitCode::FAILURE;
    }
    if let Err(error) = run() {
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
