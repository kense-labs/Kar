# Third-party ACP agents

The authenticated workbench can search the official ACP registry, install a release in its private state directory and start it through the usual project chat API. Each prepared runtime ID fixes the agent, release and distribution. Preparing a newer release gives new chats another option. Existing chats keep their saved executable, native session ID and workspace.

| Request | Result |
| --- | --- |
| `GET /acp-registry/search?query=word&limit=20` | Ranked compatible entries with distribution and integrity. Search includes names, IDs, authors and descriptions. |
| `POST /acp-registry/prepare` with `{"agent_id":"example","distribution":"auto"}` | A refreshed catalog lookup, installation, and a receipt containing `runtime`, `version`, `distribution` and `prepared`. Retrying a published installation returns the same handle. |
| `GET /acp-registry/instances` | The releases prepared in this workbench, including when the registry is offline. |
| `POST /projects/{project}/chats` with `{"id":"<uuid>","runtime":"<receipt.runtime>"}` | Starts an ACP chat. Continue through `POST /projects/{project}/chats/{chat}/messages`, including after restarting the workbench. |
| `DELETE /acp-registry/instances/{runtime}` | Removes this Kar installation once its native processes have closed. Reports `removed`. |
| `POST /acp-registry/instances/{runtime}/reinstall` | Reinstalls the recorded distribution for this exact release. The original chat can resume after reinstall. |
| `DELETE /acp-registry/agents/{agent}/managed-binary` | Cleans binary releases for an agent. Package installations and external tools stay under their respective owners. |

The default catalog is the [official ACP registry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json). Searches reuse an index for up to five minutes. Preparation always refreshes it. The host can set `KAR_ACP_REGISTRY_URL` to an HTTPS registry with the same index format. A loopback HTTP index may serve its archives on the same origin. Only the host chooses the index URL.

Automatic distribution selection follows platform binary, npx and uvx order. An explicit `binary`, `npx` or `uvx` requests that distribution. For macOS, Kar matches `darwin-aarch64` or `darwin-x86_64`. The binary can be a raw executable or a zip, tar.gz or tar.bz2 archive. The downloader checks a declared SHA-256 before extraction. Extraction and archive path validation run in Rust. npm installs an exact package in the release's dedicated prefix. uv uses its dedicated tool and executable directories. Each chat launches the installed executable with the registry arguments and environment.

A chat requires the agent to advertise ACP session load or resume. Reopening the chat uses the stored receipt and native ID, independently of the catalog and package managers. Each running ACP process holds a shared installation lock until it exits. Close it through the chat’s force-stop endpoint or by archiving the chat. Uninstall then removes the executable and its dependencies. Reading the original chat’s snapshot still works. A subsequent send requires reinstalling the pinned release. The retained metadata includes its exact package or archive URL and the downloaded binary checksum. The original distribution must still be available from that source. For earlier installations without saved distribution metadata, exact reinstall can discover the release while it is still present in the index. Binary cleanup protects a freshly prepared release for 30 seconds until a client starts it. Explicit removal of an idle instance can be immediate. A concurrent operation holding the same agent's installation lock returns HTTP 409.

## Source and verification

The behavior baseline is the workbench commit `2808e878de91aacc7fb82887f18414976be7ae34`. Its `packages/provider-acp-registry/src/server/AcpRegistrySupport.ts` lines 438, 1317, 1747, 1799 and 2073 define distribution selection, package ownership, search, preparation and managed-binary uninstall. Lines 2088–2107 check currently configured provider instances and a short-lived preparation reservation. Chat transcripts are outside the upstream reference check. Kar uses the active native process as its reference and exposes explicit removal of its own versioned installations.

| Environment | Command | What it checks | Status in development |
| --- | --- | --- | --- |
| macOS 27.2.1, Apple Silicon, Rust 1.100.0 | `cargo check --locked --all-targets` | Compiles the registry service, chat integration, test peers and both runner entries. | Passed |
| macOS with a loopback registry and Rust ACP, npm and uv peers | `cargo test --locked --test workbench_acp_registry` | All three distributions, checksum failure, declared package versions, closing, uninstalling and reading historical chats, releasing a new catalog version, reinstalling the original release, and continuing each original native conversation after a restart with the registry offline. | Compiled, awaiting consolidated execution |
| macOS with network, a selected official agent, configured provider credentials and its required package manager | `KAR_ACP_LIVE_AGENT=<agent-id> cargo test --locked --test workbench_acp_registry -- official` | Fetches the official index, installs the selected release into temporary Kar state and sends two real model turns across a service restart. The second answer must recall the first turn's random value. Prints release, native ID and platform. Optional `KAR_ACP_LIVE_DISTRIBUTION` selects the package type. | Compiled, awaiting a selected agent and account |

The locked archive dependencies add tar 0.4.46, bzip2 0.6.1 and libbz2-rs-sys 0.2.7. They use the MIT or Apache-2.0 dual license and the bzip2-1.0.6 license respectively. The package's Rust 1.89 minimum covers their declared minimum versions. Windows and Linux execution were outside this macOS development check.

## Other workbench differences identified at the fixed commit

| User operation | Fixed upstream implementation | Current Kar behavior |
| --- | --- | --- |
| Configure a local ACP command, arbitrary arguments, or a different launcher for a registry entry | `packages/provider-acp-registry/src/settings.ts` lines 10, 35 and 44, and `AcpRegistrySupport.ts` lines 1968–1981 and 2031–2052 | The workbench uses prepared catalog entries. Rust SDK callers can supply a local command through `Runtime::new`. The workbench has no general local-command profile. |
| Sign in or out of a third-party agent and select an advertised authentication method | `packages/provider-acp-registry/src/server/auth.ts` lines 381–413 and `adapter.ts` line 173 | Generic registry agents rely on existing supplier credentials. Kar’s native authentication selections in `src/session.rs` apply to named adapters. |
| See the third-party agent’s model choices before starting a chat, or configure custom choices | `packages/provider-acp-registry/src/server/probe.ts` lines 245–267 and 329–343, `settings.ts` line 61 | The generic prepared agent does not populate the workbench model picker. Kar's `src/acp_models.rs` and `src/grok.rs` select predefined adapters. |
| Start a conversation with an ACP that only advertises new sessions | `packages/provider-acp/src/server/AcpSessionRuntime.ts` lines 2446–2488 | Kar’s persistent chat API checks the native resume capability before binding a new chat. Such an agent cannot start a persistent Kar chat. |
