# MCP sign-in and provider diagnostic feedback

The fixed workbench commit is `2808e878de91aacc7fb82887f18414976be7ae34`. These actions operate on the selected project's chat and its current native provider process. A replaced or stopped controller expires the pending flow. The provider owns its credentials.

| Runtime | MCP server OAuth on the active session | Upload native diagnostic feedback |
| --- | --- | --- |
| Codex | `mcpServer/oauth/login`, callback handoff and `mcpServer/oauthLogin/completed` | `feedback/upload` with the actual native thread ID, classification `bug`, logs included and optional reason |
| Claude | `mcp_authenticate`, `mcp_oauth_callback_url`, `mcp_reconnect` | Fixed adapter exposes no diagnostic feedback uploader |
| OpenCode, Cursor, Grok, Pi, Antigravity, Kiro, Fx, Muse and custom ACP agents/profiles | Fixed adapters expose no session MCP sign-in handler | Fixed adapters expose no diagnostic feedback uploader |
| Other Kar drivers, including Kimi, Qwen, Qoder, Factory, Devin, CodeBuddy, DSH and community drivers | No corresponding handler in this fixed-workbench comparison | No corresponding uploader in this fixed-workbench comparison |

A provider that rejects a negotiated native control reports a native rejection. An absent native method produces an unsupported result where the protocol supplies a method-not-found code. The matrix describes adapter contracts. Other vendor products or account-management CLIs may expose different operations. [MCP sign-in service](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/mcpApps/McpSignIn.ts) · [Codex feedback and OAuth](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts#L9026) · [Claude OAuth](https://github.com/pingdotgg/t3code/blob/2808e878de91aacc7fb82887f18414976be7ae34/apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts#L14771)

## HTTP operations

Use `/projects/{project}/chats/{chat}` as the base. GET `/provider-controls` reports the current stream UUID and supported operations.

1. POST `/mcp-sign-in` with `{"id":"<request UUID>","stream":"<stream UUID>","server":"<native name>"}`. Supply the initiating client's private random `Kar-Auth-Client` header. A real authorization URL is returned only to that client. Reusing the request UUID recovers the current flow. An already connected server can report completion immediately.
2. POST `/mcp-sign-in/{id}` with `{"callback_url":"<full browser address>"}` and the same header. For paired clients the authenticated device must also match. Codex checks the registered origin, path, state, and a single authorization code or error. Kar sends the callback only to the negotiated loopback endpoint with redirects and proxies disabled, and waits for native success. Subsequent attempts on the same stream/server recheck native connection status to avoid attributing a late notification to the newer flow. Claude can recognize an already connected server, or accept the pasted callback and reconnect the original server. DELETE on the flow closes its local interaction. The provider controls any remaining external browser session.
3. POST `/provider-feedback` with `{"command_id":"<request UUID>","stream":"<stream UUID>","reason":"optional text"}` to authorize Codex to upload diagnostic logs. Kar returns the vendor's feedback ID. A duplicate uses the retained receipt. After a crash with an unknown outcome the same request is withheld from retransmission. Kar writes a digest and receipt. The reason, sign-in URL and callback travel outside the conversation journal. Provider errors from these private requests are reported without echoing raw diagnostic text.

## Verification

The macOS development check is `cargo check --locked --tests`. The pending centralized command is `cargo test --locked --test workbench_mcp -- provider-controls`. It starts the real Kar HTTP server, Rust Codex and Claude protocol peers, and an actual loopback callback listener. It checks cross-client denial, callback-state mismatch, native refusal, repeat completion, Claude reconnect, Codex feedback receipt and duplicate suppression, and absence of the test secrets in persistent workbench state. The peers simulate the suppliers.

Live MCP OAuth requires a compatible installed provider, a registered MCP server requiring OAuth, and an account with permission to grant that server access. On each provider, open a chat with that server and run steps 1–2, then verify it is connected in its native inventory. Live feedback requires an explicit decision to send logs from a disposable Codex thread to the vendor. On that thread, run step 3 and verify the returned vendor receipt. Live authorization and feedback upload were deferred to the designated acceptance operator.
