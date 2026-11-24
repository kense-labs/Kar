use kar_documents::extract::document_text;
use std::io::{Cursor, Write};
use zip::{ZipWriter, write::SimpleFileOptions};

fn package(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (path, text) in parts {
        zip.start_file(
            *path,
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}
#[test]
fn relationship_order_and_visible_content() {
    let word = package(&[
        (
            "_rels/.rels",
            r#"<Relationships><Relationship Id="chosen" Type="r/officeDocument" Target="/word/document2.xml"/></Relationships>"#,
        ),
        (
            "word/document2.xml",
            r#"<document><p><t>Selected</t><del><t>deleted</t></del><tab/><t>line</t></p></document>"#,
        ),
        ("word/document.xml", "<document><p>stale</p></document>"),
    ]);
    assert_eq!(
        document_text("doc.docx", &word).as_deref(),
        Some("Selected\tline")
    );

    let epub = package(&[
        (
            "META-INF/container.xml",
            r#"<container><rootfile full-path="text/package.opf"/></container>"#,
        ),
        (
            "text/package.opf",
            r#"<package><manifest><item id="first" href="a%20page.xhtml"/><item id="second" href="b.xhtml"/></manifest><spine><itemref idref="second"/><itemref idref="first"/></spine></package>"#,
        ),
        (
            "text/a page.xhtml",
            "<html><head><title>hidden title</title></head><body><p>End</p></body></html>",
        ),
        ("text/b.xhtml", "<html><body><p>Start</p></body></html>"),
    ]);
    assert_eq!(
        document_text("book.epub", &epub).as_deref(),
        Some("## Chapter 1\nStart\n\n## Chapter 2\nEnd")
    );
}
#[test]
fn open_document_text_sheet_and_slide() {
    let ns = r#"xmlns:o="urn:oasis:names:tc:opendocument:xmlns:text:1.0""#;
    let sample = |tag: &str, inner: &str| {
        package(&[(
            "content.xml",
            &format!("<root {ns}><{tag}>{inner}</{tag}></root>"),
        )])
    };
    let odt = sample(
        "body",
        r#"<o:h o:outline-level="2">Notes</o:h><o:p>left<o:tab/>right</o:p>"#,
    );
    assert_eq!(
        document_text("notes.odt", &odt).as_deref(),
        Some("## Notes\nleft\tright")
    );
    let ods = sample(
        "body",
        r#"<o:table o:name="Units"><o:table-row o:number-rows-repeated="2"><o:table-cell o:number-columns-repeated="2" o:value-type="boolean" o:boolean-value="true"/></o:table-row></o:table>"#,
    );
    assert_eq!(
        document_text("units.ods", &ods).as_deref(),
        Some("## Sheet: Units\ntrue\ttrue\ntrue\ttrue")
    );
    let odp = sample(
        "body",
        "<o:page><o:p>Slide body</o:p><o:notes><o:p>Speaker</o:p></o:notes></o:page>",
    );
    assert_eq!(
        document_text("deck.odp", &odp).as_deref(),
        Some("## Slide 1\nSlide body\n\nNotes:\nSpeaker")
    );
}
#[test]
fn notebook_rtf_and_failure_limits() {
    let notebook=br#"{"cells":[{"cell_type":"code","source":["show()"],"outputs":[{"data":{"image/png":"hidden","text/plain":["\u001b[31mhello","\u001b[0m"]}}]}]}"#;
    assert_eq!(
        document_text("code.ipynb", notebook).as_deref(),
        Some("## Cell 1 (code)\nshow()\nOutput:\nhello")
    );
    assert_eq!(
        document_text(
            "notes.rtf",
            br#"{\rtf1\ansi shown {\fonttbl private;}\uc1\u20320?\par next}"#
        )
        .as_deref(),
        Some("shown 你\nnext")
    );
    assert!(document_text("broken.docx", b"broken").is_none());
    assert!(document_text("broken.rtf", b"{\\rtf1 open").is_none());
    assert!(document_text("oversize.ipynb", &vec![b' '; 16 * 1024 * 1024 + 1]).is_none());
    let big = serde_json::json!({"cells":[{"source":"🦀".repeat(510000)}]}).to_string();
    let small = document_text("large.ipynb", big.as_bytes()).unwrap();
    assert!(small.contains("truncated"));
    assert!(small.encode_utf16().count() < 1_000_100);
}
