# Prepared runs

The owner creates a launch with `PUT /projects/{project}/chats/{chat}/prepared-runs/{run}`. The chat and run UUIDs are chosen by the client and survive retries and Kar restarts. The response includes a sequence, attempt and stage. Use `GET /projects/{project}/prepared-runs` to find accepted launches, including launches whose worktree or native chat is still being created.

```json
{
  "launch": {"runtime":"codex"},
  "title": "Review the release",
  "workspace": {"type":"worktree","directory":"/authorized/tasks/release","branch":"review-release","base":"HEAD","from_origin":false},
  "message": "Review the release checklist",
  "requested_output_schema": {"type":"object"},
  "defer_release": true
}
```

Omit `launch` to prepare input in an existing chat's bound workspace. A new chat accepts a root, an existing checkout from the same repository, or a managed worktree. The ordinary model tool `kar_thread_launch` uses the same worker and stable initial input ID. It inherits the caller's provider restrictions. Referenced model attachments must be visible in the calling chat. An owner can refer to attachment IDs already staged for the target chat. A run keeps its original request across retries.

The worker persists worktree preparation, setup, ready, releasing, released and failed transitions. The first host-configured worktree creation script runs in a tracked terminal in the prepared checkout. By default its successful exit gates release. A script marked async permits release after launch, with subsequent output and exit available on the terminal. Kar stores the terminal ID before starting the script. If Kar lost the exit status, synchronous setup fails visibly. A retry is refused while the previous identified terminal process still lives.

`GET` on the same run reads its status. `GET .../events` replays transitions after the given `Last-Event-ID` or `after` query, then streams changes. A failed attempt stays failed until an owner uses `POST .../retry` with a fresh `command_id`. Retrying workspace setup reads the current project script and retains the accepted workspace request. A release failure retries dispatch using the existing prepared checkout. A held run waits at ready until `POST .../release` with a `command_id`. Replayed commands and simultaneous releases reuse the same input, including its schema and attachments. Queue reconciliation determines whether the native provider accepted it.

The macOS handoff scenario builds a real Git workspace, runs a failing PTY setup script, restarts Kar, fixes the configured script, retries from two clients, restarts at ready, and releases the same run from both clients. A disposable native Codex protocol peer provides receipts. Run `cargo test --locked --test workbench_mcp -- prepared-runs`. The fixed workbench counterpart is [ThreadLaunchService](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/orchestration-v2/ThreadLaunchService.ts).
