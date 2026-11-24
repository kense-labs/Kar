# Kar headless Office edits

The Rust library uses Kantor revision `9b780379e32d3fa4d293499f33fbc2483655567d` for its document, spreadsheet and presentation models, including Kantor's IronCalc patches. All three engines load without opening a window.

## Engine round-trip verification

On macOS 27.2.1 arm64 with rustc 1.100.0, the following command passed nine tests across the three formats.

```sh
cargo test --locked --manifest-path crates/kar-documents/Cargo.toml
```

| File | Edits and checks performed |
| --- | --- |
| DOCX | Edit two Chinese ranges in one paragraph, reject a competing replacement and invalid UTF-8 offset, save and reopen both paragraphs. Resolve two identical paragraphs after an insertion, independently check bold in document.xml, exercise both arrival orders and detect deletion and ambiguous splits. |
| XLSX | Reorder sheets, merge B3 value and fill with D3 formula in 预算, retry the same edit, detect competing formula input, save and reopen. Verify D3 computes 36, the second sheet retains 99, and worksheet XML contains the formula and cached result. |
| PPTX | Reorder slides and layers, edit text and x position on one of two identically named shapes, retry, and detect conflicting position and deleted target. Reopen and verify both shapes, selected object's XML, image bytes and relationship part. |

`Cell::apply` and `Object::apply` use the current Kantor model and compare the addressed property against its base and proposed values. They return an applied/unchanged outcome or a typed conflict with its target, property and values. A caller can inspect a conflict with `anyhow::Error::downcast_ref::<kar_documents::Conflict>()`.

`prepare(base_bytes, current_bytes, edits)` opens isolated copies of both revisions. It validates the selected properties against the base and applies a complete batch to the current snapshot. It returns candidate Office bytes after the entire batch succeeds, or `None` for an equivalent edit. A conflict after an earlier compatible edit discards the batch. Candidate tests reopen all three formats and check a failed batch leaves the original bytes intact.

For a stale spreadsheet proposal, `prepare_with_grid_history` takes committed row/column inserts and deletes from the service between the two revisions. It replays those changes in the base and proposed models to translate addresses and Excel formulas. An empty journal asserts the service saw no structural changes. With an unknown external save, `prepare` reports an unresolved target. Tests check B3/D3 move to B4/D4, the new formula is =B4*3 with a cached result of 36, and a deleted logical cell conflicts even when its neighbor holds the same original value.

These tests exercise the document library. The shared service still needs to associate it with revisions, operation IDs, persistent receipts, disk publication and subscriber cursors.

## Document transactions

`DocumentStore::open(state_dir, working_file)` registers a single Office file. A store holds an exclusive writer lock. Its manifest records immutable SHA-256 snapshots, author and operation ID, the command, a receipt and an ordered cursor. Retrying an acknowledged or unacknowledged command returns its original receipt. Reusing its ID for another payload is rejected.

Each accepted edit persists a publication intent and snapshot, atomically replaces the working file, then finalizes the revision and receipt. Reopening resumes an incomplete publication. An observed external save becomes a revision. When a pending intent encounters different external bytes it retains both versions and records a conflict. An external writer racing the last comparison and rename still requires workspace-wide write coordination.

`Action::Resolve` accepts the conflict cursor, decision and latest reviewed revision. Keep-current, adopt-proposed and manual Office edits preserve unrelated properties. A stale resolution returns the current revision for review. `state`, `events(after)` and `snapshot(revision)` return persisted results.

The macOS transaction tests exercised all three decisions, stale resolution, retry after reopening, operation-ID collisions, an intervening external editor save and three reconstructed durable publication windows. The reconstructed windows cover a stop before file replacement, a stop after replacement, and an external edit against pending intent.

```sh
cargo test --locked --manifest-path crates/kar-documents/Cargo.toml --test transactions
```

## Local service

```sh
cargo run --locked --manifest-path crates/kar-documents/Cargo.toml -- --file /path/to/notes.docx --state /path/to/document-state
```

The process prints the loopback address and path of a local bearer token. Clients send the token in the Authorization header. A single service serves one working file. It accepts JSON commands at `POST /operations`, reports the head at `GET /state` and returns exact file bytes at `GET /revisions/{revision}`. `GET /events?after=12&wait_ms=25000` waits for the next receipt and can resume from the same cursor after a service restart. A property conflict returns HTTP 409 with the persisted receipt. The local bearer token grants access to the document. The author field identifies the contributor supplied by that client.

The `http_clients` integration test is prepared for final acceptance. It starts a real service subprocess, connects two independent TCP clients, submits simultaneous edits, waits on a subscription, loses a response, stops and restarts the process, resumes and settles a conflict. It has only been compiled in this implementation phase.

## Text and native-file proposals

The same service accepts UTF-8 `.txt`, `.md` and `.markdown`. `DocumentEdits::Text` carries byte ranges in the base snapshot. The line and Unicode scalar diff preserves unchanged line endings, BOM and Markdown content.

A native editor writes its complete candidate to an isolated file and uploads the bytes to `POST /proposals` (up to 128 MiB per request). `GET /proposals/{sha256}` retrieves the exact staged bytes, including an unresolved proposal after restart. A normal `save` command references the returned SHA-256, operation ID, author and base revision. A current-revision save uses the validated proposed file. A stale Office save derives paragraph, cell or shape changes, replays them against the base, and checks every uncompressed ZIP part against the candidate before merging. Unknown or lossy changes return a conflict containing the file references. A manual-file resolution references a staged whole document reviewed against the current revision.

A current-revision `grid` command records each managed row/column insertion or deletion. Stale cell and formula edits replay this history before publication. Unknown external workbook structure remains a conflict.

The macOS target check compiled the text/native-file scenarios in `native_files`. Execution is pending consolidated acceptance.

`DocumentEdits::WordDocument` applies a batch of text and direct character-format changes. Bold, italic and underline preserve the distinction between an explicitly disabled attribute and inheriting it from the paragraph style. Character replacement preserves the selected run's properties. Selections that cross incompatible run formatting or protected inline objects need an explicit narrower edit. PowerPoint text, solid RGB fill, x/y and width/height compare independently. Unsupported gradients or pattern fills retain their original value and produce a conflict when a solid fill cannot be compared.

The prepared `properties` tests cover a Word run boundary and edits to a slide's text and fill. The `http_clients` scenarios now include all three Office formats in human-first, AI-first and barrier-synchronized submission orders, with conflict hashes, reconnect, retry and explicit resolution. They are awaiting the consolidated scenario run.
