# OpenCode and Cursor background commands on macOS

The workbench reference is commit `2808e878de91aacc7fb82887f18414976be7ae34`. Kar owns the Rust session controller and reads the native provider's evidence for the selected root session.

## OpenCode 2

The fixed workbench opens the v2 server directly. It takes the shell ID from a successful tool return, reads outstanding native jobs on attach and settles work from the session's synthetic shell notice. Its stop request runs on that same server. [v2 adapter](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-opencode/src/server/v2/adapter.ts) · [job list](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-opencode/src/server/v2/OpenCode2Client.ts#L284)

Kar opens the native ACP CLI. A root execute-tool receipt with `rawOutput.metadata.shellID` and `status: running` identifies the background command. The foreground tool can already be complete. Native job recovery records identify the job alias, command and owner. Kar reads the committed SQLite/WAL job table, pending synthetic inbox notices and delivered synthetic messages. A missing job row leaves the outcome pending. Completion requires native evidence. The reader uses the session's effective data directory and respects `OPENCODE_DB`. In-memory databases and custom channel filenames without an explicit path cannot supply settlement through this reader. Unreadable or over-limit native databases also leave work pending. [2.2.7 shell tool](https://github.com/anomalyco/opencode/blob/v2.2.7/packages/core/src/tool/plugin/shell.ts) · [2.3.2 ACP tool](https://github.com/anomalyco/opencode/blob/v2.3.2/packages/cli/src/acp/tool.ts#L101) · [ACP inbox translation](https://github.com/anomalyco/opencode/blob/v2.3.2/packages/cli/src/acp/translate.ts#L132) · [native job store](https://github.com/anomalyco/opencode/blob/v2.3.2/packages/core/src/job.ts) · [native terminal notice](https://github.com/anomalyco/opencode/blob/v2.3.2/packages/core/src/shell/result.ts#L75)

From v2.2.7 within major 2, `Session::tasks`, `Session::await_background_tasks`, the workbench background task routes and the attached model's `kar_background_wait` use the native shell ID. Wait also accepts an observed job alias or a verified, still-live local PID. OpenCode 2.2.7's published shell metadata omits the PID. OpenCode 1 uses a different schema and transport behavior.

From v2.3.2, a local macOS shell receipt may include a PID. Kar records the process birth identity only when it observes the PID below the owning ACP process. Stop rechecks the controller, shell occurrence, process birth and native detached process group, then sends TERM. It may escalate after three seconds if that same group leader remains owned. The task settles when the provider supplies terminal evidence. Reattached or remote shells with no verified local group reject stop. Kar's normal ACP process creates a separate private server, so a successful delete on a second standalone server cannot prove the shell stopped. Native same-server stopping for a shell without local ownership would require a transport attached to the original server. [2.3.2 launch receipt](https://github.com/anomalyco/opencode/blob/v2.3.2/packages/core/src/tool/plugin/shell.ts#L250) · [local shell spawner](https://github.com/anomalyco/opencode/blob/v2.3.2/packages/core/src/shell.ts#L955) · [standalone server](https://github.com/anomalyco/opencode/blob/v2.3.2/packages/cli/src/services/standalone.ts)

Select an occurrence using `GET /projects/{project}/chats/{chat}/background-tasks`. Send its ID and optional start cursor to `POST .../background-tasks/await`. To stop it, send its `{"started_at": <cursor>}` to `POST .../background-tasks/{id}/stop`. Reusing a cursor from another stream or occurrence is refused.

## Cursor

The fixed workbench's Cursor SDK 1.4.2 patch installs a per-conversation callback at `Symbol.for("@cursor/sdk.backgroundShell")`. The handle provides its shell ID, optional PID, native abort method and completion. Kar starts Cursor's `agent acp`; the investigated ACP adapter has no corresponding shell handle, owned PID or stop callback. Child chat IDs and displayed terminal text cannot establish shell ownership. Cursor ACP continues to offer its supported chat controls. Adding parity for these shell controls needs an equivalent official ACP extension or an adapter that owns the SDK shell handles. [fixed patch](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/patches/%40cursor__sdk%401.4.2.patch) · [SDK hook](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-cursor/src/server/CursorAgentSdk.ts#L555) · [wait and stop](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/packages/provider-cursor/src/server/adapter.ts#L3400)

## Verification

Development scope is macOS compilation. These commands are queued for the designated acceptance run. The first uses constructed ACP frames and SQLite rows. The next two run the actual OpenCode CLI with a loopback model, disposable data directories and a shell owned by the real provider. A missing launch, native PID, or completion fails the corresponding case.

```sh
cargo test --locked --lib transport::background::shells::tests
KAR_OPENCODE_BIN=/absolute/path/to/opencode-2.2.7 cargo test --locked --test native_live native_background_shell_receipt_wait_and_terminal -- --ignored --nocapture
KAR_OPENCODE_BIN=/absolute/path/to/opencode-2.3.2 KAR_OPENCODE_SHELL_STOP=1 cargo test --locked --test native_live native_background_shell_receipt_wait_and_terminal -- --ignored --nocapture
```
