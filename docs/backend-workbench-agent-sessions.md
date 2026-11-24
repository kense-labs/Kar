# Native session discovery and import

The owner API exposes `GET /agent-sessions/scan` and `POST /projects/{project}/agent-sessions/import`. The import body accepts an optional `expected_workspace_root`. The service reads the Codex, Claude and Pi homes configured for its process. It checks workspace authorization, groups verified linked worktrees with their primary checkout and resumes an imported chat in the checkout that held the conversation. The result includes new, already present and skipped counts, retained Kar chat IDs and a truncation flag. Times use epoch milliseconds.

The fixed workbench revision is `2808e878de91aacc7fb82887f18414976be7ae34`. The source entries are `apps/server/src/project/AgentSessionScanner.ts`, `apps/server/src/project/AgentSessionImporter.ts`, their adjacent tests and `packages/contracts/src/agentSessions.ts`.

| Source | Native files | Import and restart behavior |
| --- | --- | --- |
| Codex | `CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl[.zst]`, `session_index.jsonl`, `state_5.sqlite` and its WAL | Reads the native thread ID, title and model. Excludes subagents, deduplicates canonical and response copies of a user message within the same turn, and reads inherited rollout byte prefixes when available. A scratch cwd can resolve to the first project root recorded in SQLite. Reopen uses the native thread ID. |
| Claude | `CLAUDE_CONFIG_DIR/projects/{directory}/*.jsonl` | Uses the native session UUID, visible conversation, model and saved title. Filters sidechains, setup/output records and synthetic-only failures. Reopen uses the validated native UUID. |
| Pi | `KAR_PI_AGENT_DIR/sessions` or `PI_CODING_AGENT_DIR/sessions`, including flat and nested JSONL | Follows the last leaf through parent IDs, preserves text on the active branch and retains the latest session name. Reopen passes the exact source file to Pi and verifies the session UUID and cwd. |

The defaults are the native per-user homes. Scanning does not start a provider. Import reads sessions modified in the last 30 days. It retains at most 200 messages with the first user message and recent history. The Kar chat header records the original source identity and omitted count. Imports acquire the chat lease, publish the journal atomically and record project ownership under the catalog lock. A changed transcript does not overwrite a continued chat. Discovery and import report budget truncation. Safe reimport of unchanged files does not consume the per-request transcript allowance.

Kar uses `ruzstd` 0.9.1 (MIT, Rust 1.87) for Zstandard decoding and `sqlite-core` 0.12.0 (Apache-2.0, Rust 1.80) for read-only SQLite and WAL projection. The JSONL selector uses `struson` 0.7.2 (MIT or Apache-2.0, Rust 1.85). Kar currently declares Rust 1.89. The dependency versions are locked.

## Provider instances

`KAR_AGENT_SESSION_CONFIG` can point the server to a host-owned TOML file. Each entry has a stable instance ID and a native runtime. Optional `home`, `program` and Pi `sessions` paths must be absolute. The default instance names are `codex`, `claude` and `pi`; setting `enabled = false` disables one. Reads are confined to the configured session directories. Instances sharing the same physical directory are scanned once, with the default preferred. Kar scopes native IDs to an instance and pins its home for future reopen. Credentials remain with the native provider.

```toml
[instances.pi-personal]
runtime = "pi"
home = "/Users/example/.pi-personal"
sessions = "/Users/example/.pi-personal/history"

[instances.codex-work]
runtime = "codex"
home = "/Users/example/.codex-work"
```

`kar::provider_instance::HistoryInstance` identifies the runtime, native home, session directory and optional executable. The scanner and saved chat binding share it. The HTTP fixture includes two Pi profiles using the same native UUID and a second configuration alias pointing at the default profile.

## Unified verification

`cargo test --locked --test workbench_agent_sessions` starts a real Kar service with three isolated provider-file fixtures. It covers root filtering, subagent exclusion, the compressed Codex projection, active Pi branch, Claude sidechains, stale project roots, restart and replay, refusal to overwrite changed history and delivery into the original Codex session via an isolated protocol peer. The workbench suite includes this test in its local group.

The implementation and the scenario passed macOS `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 cargo check --locked --all-targets`. The HTTP scenario and installed-provider verification await unified execution. No real-provider or hosted service was contacted by the scenario during development.

## Boundaries to verify

The service limits optional index snapshots to 128 MiB for SQLite and 64 MiB for its WAL. Inaccessible or unstable indexes leave the native cwd in effect. The JSONL reader skips unselected image and tool bodies while parsing the surrounding record. The retained history and selected record fields share a 32 MiB allowance. Exhausting it skips the transcript. Claude retains the first user message and a sliding message window while streaming. `cargo test --locked --lib workbench::agent_sessions::jsonl::tests` covers a 36 MiB unselected field, malformed records and selected-field exhaustion. Unusually large SQLite databases and large collections of projects need separate coverage. Kar can resume from imported text. Imported historical turns have no Kar Git checkpoints.
