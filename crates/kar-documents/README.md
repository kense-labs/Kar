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
