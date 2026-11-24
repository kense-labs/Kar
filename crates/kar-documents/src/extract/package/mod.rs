mod sheet;
mod visible;
use super::{Text, Truncated};
use anyhow::{Result, anyhow, bail, ensure};
use roxmltree::{Document, Node, ParsingOptions};
use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
};
use zip::ZipArchive;

type Parts = BTreeMap<String, String>;
fn read_parts(data: &[u8], out: &Text) -> Result<Parts> {
    let mut zip = ZipArchive::new(Cursor::new(data))?;
    if zip.len() > 2048 {
        return Err(Truncated.into());
    }
    let mut files = Parts::new();
    let mut remaining = 32 * 1024 * 1024u64;
    for i in 0..zip.len() {
        out.check()?;
        let file = zip.by_index(i)?;
        ensure!(!file.encrypted(), "encrypted document");
        let name = file.name()?.into_owned();
        if !name.rsplit_once('.').is_some_and(|(_, e)| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "xml" | "rels" | "html" | "xhtml" | "htm" | "opf" | "svg"
            )
        }) {
            continue;
        }
        let size = file.size();
        if size > remaining || size > 1024.max(file.compressed_size().saturating_mul(200)) {
            return Err(Truncated.into());
        }
        remaining -= size;
        let mut buf = Vec::new();
        file.take(size + 1).read_to_end(&mut buf)?;
        ensure!(buf.len() as u64 == size, "incorrect document member size");
        let codec = if buf.starts_with(&[0xff, 0xfe]) || buf.starts_with(&[b'<', 0]) {
            encoding_rs::UTF_16LE
        } else if buf.starts_with(&[0xfe, 0xff]) || buf.starts_with(&[0, b'<']) {
            encoding_rs::UTF_16BE
        } else {
            encoding_rs::UTF_8
        };
        let (text, _, bad) = codec.decode(&buf);
        ensure!(!bad, "invalid XML encoding");
        ensure!(
            files.insert(name, text.into_owned()).is_none(),
            "ambiguous document member"
        );
    }
    Ok(files)
}
fn xml<'a>(files: &'a Parts, path: &str) -> Result<Document<'a>> {
    let src = files
        .get(path)
        .ok_or_else(|| anyhow!("missing document part"))?;
    Ok(Document::parse_with_options(
        src,
        ParsingOptions {
            nodes_limit: 450_000,
            ..Default::default()
        },
    )?)
}
fn nodes<'a, 'b>(n: Node<'a, 'b>, tag: &'static str) -> impl Iterator<Item = Node<'a, 'b>> {
    n.descendants()
        .filter(move |n| n.is_element() && n.tag_name().name() == tag)
}
fn attr<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    n.attributes().find(|a| a.name() == name).map(|a| a.value())
}
fn resolve(part: &str, target: &str) -> Result<String> {
    ensure!(
        !target.contains(['\\', ':', '\0']),
        "invalid package reference"
    );
    let mut path = Vec::new();
    if !target.starts_with('/')
        && let Some((dir, _)) = part.rsplit_once('/')
    {
        path.extend(dir.split('/'));
    }
    for c in target.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                ensure!(path.pop().is_some(), "reference outside package");
            }
            s => path.push(s),
        }
    }
    Ok(path.join("/"))
}
struct Relation {
    target: String,
    kind: String,
}
fn rels(files: &Parts, part: &str) -> Result<BTreeMap<String, Relation>> {
    let (dir, name) = part.rsplit_once('/').map_or(("", part), |(d, f)| (d, f));
    let path = if dir.is_empty() {
        format!("_rels/{name}.rels")
    } else {
        format!("{dir}/_rels/{name}.rels")
    };
    let mut result = BTreeMap::new();
    for n in nodes(xml(files, &path)?.root(), "Relationship") {
        if attr(n, "TargetMode") == Some("External") {
            continue;
        }
        if let (Some(id), Some(t), Some(kind)) = (attr(n, "Id"), attr(n, "Target"), attr(n, "Type"))
        {
            ensure!(
                result
                    .insert(
                        id.into(),
                        Relation {
                            target: resolve(part, t)?,
                            kind: kind.into()
                        }
                    )
                    .is_none(),
                "duplicate relationship"
            );
        }
    }
    Ok(result)
}
fn relation_id<'a>(n: Node<'a, '_>) -> Option<&'a str> {
    n.attributes()
        .find(|a| {
            a.name() == "id"
                && a.namespace()
                    .is_some_and(|ns| ns.ends_with("/relationships"))
        })
        .map(|a| a.value())
        .or_else(|| attr(n, "id"))
}
fn office_main(files: &Parts, fallback: &str) -> String {
    rels(files, "")
        .ok()
        .and_then(|r| {
            r.into_values()
                .find(|r| r.kind.ends_with("/officeDocument") && files.contains_key(&r.target))
        })
        .map(|r| r.target)
        .unwrap_or_else(|| fallback.into())
}
pub(super) fn extract(ext: &str, data: &[u8], out: &mut Text) -> Result<()> {
    let files = read_parts(data, out)?;
    match ext {
        "docx" => {
            let doc = xml(&files, &office_main(&files, "word/document.xml"))?;
            visible::body(doc.root(), out)
        }
        "xlsx" => sheet::excel(&files, out),
        "pptx" => {
            let main = office_main(&files, "ppt/presentation.xml");
            let presentation = xml(&files, &main)?;
            let rels_by_id = rels(&files, &main)?;
            for (i, slide) in nodes(presentation.root(), "sldId").enumerate() {
                let part = relation_id(slide)
                    .and_then(|id| rels_by_id.get(id))
                    .ok_or_else(|| anyhow!("missing slide relationship"))?;
                out.push(&format!("\n\n## Slide {}\n", i + 1))?;
                visible::body(xml(&files, &part.target)?.root(), out)?;
                if let Ok(notes) = rels(&files, &part.target) {
                    for r in notes.values().filter(|r| r.kind.ends_with("/notesSlide")) {
                        let doc = xml(&files, &r.target)?;
                        out.push("\nNotes:\n")?;
                        visible::body(doc.root(), out)?;
                    }
                }
            }
            Ok(())
        }
        "odt" | "ods" | "odp" => {
            let doc = xml(&files, "content.xml")?;
            if ext == "odt" {
                return visible::body(doc.root(), out);
            }
            for (i, part) in nodes(doc.root(), if ext == "ods" { "table" } else { "page" })
                .filter(|n| {
                    !n.ancestors()
                        .skip(1)
                        .any(|a| a.is_element() && a.tag_name() == n.tag_name())
                })
                .enumerate()
            {
                if ext == "ods" {
                    out.push(&format!(
                        "\n## Sheet: {}\n",
                        attr(part, "name").unwrap_or_default()
                    ))?;
                    visible::table(part, out)?;
                } else {
                    out.push(&format!("\n## Slide {}\n", i + 1))?;
                    visible::body(part, out)?;
                }
            }
            Ok(())
        }
        "epub" => {
            let container = xml(&files, "META-INF/container.xml")?;
            let root = nodes(container.root(), "rootfile")
                .find_map(|n| attr(n, "full-path"))
                .ok_or_else(|| anyhow!("missing EPUB spine"))?;
            let book = xml(&files, root)?;
            let manifest = nodes(book.root(), "item")
                .filter_map(|n| Some((attr(n, "id")?, attr(n, "href")?)))
                .collect::<BTreeMap<_, _>>();
            for (i, item) in nodes(book.root(), "itemref").enumerate() {
                let loc = attr(item, "idref")
                    .and_then(|id| manifest.get(id))
                    .ok_or_else(|| anyhow!("missing chapter"))?;
                let target =
                    percent_encoding::percent_decode_str(loc.split('#').next().unwrap_or_default())
                        .decode_utf8()?;
                let p = resolve(root, &target)?;
                out.push(&format!("\n\n## Chapter {}\n", i + 1))?;
                visible::body(xml(&files, &p)?.root(), out)?;
            }
            Ok(())
        }
        _ => bail!("unsupported document"),
    }
}
