use super::*;
use docs_engine::{Document, Selection};
use kar_documents::{
    Change, OfficeEdits,
    excel::{Cell, CellEdit},
    slides::{Object, ObjectEdit},
    word::WordEdit,
};
use sheets_engine::Workbook;
use slides_engine::{
    Presentation, Xfrm,
    author::{Element, ElementKind, GeomSpec, ShapeSpec, TextSpec},
};
pub const DOCUMENTS: [&str; 3] = ["plan.docx", "plan.xlsx", "plan.pptx"];
fn path(tree: &Path, f: &str) -> PathBuf {
    tree.join(f)
}
fn shape(text: &str, x: i64) -> Element {
    Element {
        name: Some(text.into()),
        xfrm: Xfrm::new(x, 100, 200_000, 100_000),
        kind: ElementKind::Shape(ShapeSpec {
            geom: GeomSpec::preset("rect"),
            text_box: true,
            fill: None,
            line: None,
            text: Some(TextSpec::plain(text)),
            style: Default::default(),
        }),
    }
}
pub fn seed(tree: &Path) {
    let mut doc = Document::new();
    doc.insert_text(Selection::default(), "left\nright", None);
    doc.save_as(&path(tree, DOCUMENTS[0])).unwrap();
    let mut book = Workbook::new_empty().unwrap();
    book.set_input(0, 0, 0, "left").unwrap();
    book.set_input(0, 0, 1, "right").unwrap();
    book.save_as(path(tree, DOCUMENTS[1])).unwrap();
    let mut deck = Presentation::new();
    deck.add_element(0, &shape("left", 100)).unwrap();
    deck.add_element(0, &shape("right", 400)).unwrap();
    deck.save_as(&path(tree, DOCUMENTS[2])).unwrap();
}
fn edit(tree: &Path, file: &str, lane: usize, old: &str, new: &str) -> OfficeEdits {
    let change = Change {
        base: old.into(),
        proposed: new.into(),
    };
    match file {
        "plan.docx" => OfficeEdits::Word(vec![WordEdit {
            paragraph: lane,
            range: 0..old.len(),
            replacement: new.into(),
        }]),
        "plan.xlsx" => {
            let book = Workbook::open(path(tree, file)).unwrap();
            OfficeEdits::Excel(vec![(
                Cell::at(&book, 0, 0, lane as u32).unwrap(),
                CellEdit::Input(change),
            )])
        }
        _ => {
            let deck = Presentation::open(&path(tree, file)).unwrap();
            let offset = deck.slide(0).shapes.len() - 2;
            OfficeEdits::Slides(vec![(
                Object::at(&deck, 0, offset + lane).unwrap(),
                ObjectEdit::Text(change),
            )])
        }
    }
}
pub fn verify(tree: &Path, file: &str, left: &str, right: &str) {
    let path = tree.join(file);
    let values = match file {
        "plan.docx" => Document::open(&path)
            .unwrap()
            .paragraphs()
            .iter()
            .map(|p| p.text.clone())
            .collect::<Vec<_>>(),
        "plan.xlsx" => {
            let b = Workbook::open(&path).unwrap();
            vec![b.input(0, 0, 0), b.input(0, 0, 1)]
        }
        _ => {
            let deck = Presentation::open(&path).unwrap();
            let shapes = &deck.slide(0).shapes;
            shapes[shapes.len() - 2..]
                .iter()
                .map(|s| s.text.as_ref().unwrap().plain_text())
                .collect()
        }
    };
    assert_eq!(values, [left, right], "{file}: re-opened from disk");
}
pub async fn submit(a: &Client, b: &Client, scope: &str, tree: &Path) -> Vec<Value> {
    let mut evidence = vec![];
    for file in DOCUMENTS {
        let route = format!("{scope}/documents/operations?path={file}");
        let before = ok(a.get(format!("{scope}/documents/state?path={file}"))).await;
        assert_eq!(before["revision"], 0);
        let op = |who: &str, lane: usize, old: &str, new: &str| json!({"author":who,"operation_id":format!("{file}-{new}"),"base_revision":0,"action":{"type":"edit","edits":edit(tree,file,lane,old,new)}});
        let human = op("owner", 0, "left", "human");
        let ai = op("assistant", 1, "right", "model");
        let collision = op("assistant", 0, "left", "other");
        let (h, m) = tokio::join!(
            ok(a.post(&route).json(&human)),
            ok(b.post(&route).json(&ai))
        );
        assert_eq!(h["status"], "applied");
        assert_eq!(m["status"], "applied");
        assert_ne!(h["revision"], m["revision"]);
        verify(tree, file, "human", "model");
        let conflict = b.post(&route).json(&collision).send().await.unwrap();
        assert_eq!(conflict.status(), 409);
        let conflict: Value = conflict.json().await.unwrap();
        assert_eq!(conflict["revision"], 2);
        verify(tree, file, "human", "model");
        evidence.push(
            json!({"file":file,"human":h,"assistant":m,"conflict":conflict,"request":collision}),
        );
    }
    evidence
}
pub async fn finish(c: &Client, scope: &str, tree: &Path, rows: &[Value]) -> Vec<Value> {
    let mut results = vec![];
    for entry in rows {
        let file = entry["file"].as_str().unwrap();
        verify(tree, file, "human", "model");
        let route = format!("{scope}/documents/operations?path={file}");
        let again = c.post(&route).json(&entry["request"]).send().await.unwrap();
        assert_eq!(again.status(), 409);
        assert_eq!(again.json::<Value>().await.unwrap(), entry["conflict"]);
        let resolve = json!({"author":"owner","operation_id":format!("{file}-resolve"),"base_revision":2,"action":{"type":"resolve","conflict":entry["conflict"]["conflict_id"],"decision":{"type":"keep_current"}}});
        let receipt = ok(c.post(&route).json(&resolve)).await;
        assert_eq!(receipt["resolves"], entry["conflict"]["conflict_id"]);
        assert_eq!(receipt, ok(c.post(&route).json(&resolve)).await);
        verify(tree, file, "human", "model");
        results.push(json!({"file":file,"accepted_revisions":[entry["human"]["revision"],entry["assistant"]["revision"]],"conflict":entry["conflict"]["conflict_id"],"resolution":receipt,"disk_values":["human","model"]}));
    }
    results
}
