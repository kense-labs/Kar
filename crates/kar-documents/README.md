# Kar headless Office edits

The Rust library uses Kantor revision `9b780379e32d3fa4d293499f33fbc2483655567d` for its document, spreadsheet and presentation models, including Kantor's IronCalc patches. All three engines load without opening a window.

## Engine round-trip verification

On macOS 27.2.1 arm64 with rustc 1.100.0, the following command passed one Word test and two integration tests.

```sh
cargo test --locked --manifest-path crates/kar-documents/Cargo.toml
```

| File | Edits and checks performed |
| --- | --- |
| DOCX | Edit two Chinese ranges in one paragraph, reject a competing replacement and invalid UTF-8 offset, save and reopen both paragraphs. |
| XLSX | Reorder sheets, merge B3 value and fill with D3 formula in 预算, retry the same edit, detect competing formula input, save and reopen. Verify D3 computes 36, the second sheet retains 99, and worksheet XML contains the formula and cached result. |
| PPTX | Reorder slides and layers, edit text and x position on one of two identically named shapes, retry, and detect conflicting position and deleted target. Reopen and verify both shapes, selected object's XML, image bytes and relationship part. |

`Cell::apply` and `Object::apply` use the current Kantor model and compare the addressed property against its base and proposed values. They return an applied/unchanged outcome or a typed conflict with its target, property and values. A caller can inspect a conflict with `anyhow::Error::downcast_ref::<kar_documents::Conflict>()`.

These tests exercise the document library. The shared service still needs to associate it with revisions, operation IDs, persistent receipts, disk publication and subscriber cursors.
