//! The read and write tools parse documents in a `loop __doc-to-text`
//! child, so hostile files cannot stop this process. Every fixture is
//! built here, small, from bytes.

use std::io::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use loop_agent::harness::{
    create_read_tool, create_write_tool, set_document_reader, DocumentReader, ExecutionEnv,
    HostExecutionEnv, DOCUMENT_READER_ARG,
};
use serde_json::json;

fn setup() -> (tempfile::TempDir, Arc<dyn ExecutionEnv>) {
    set_document_reader(DocumentReader::Child {
        program: env!("CARGO_BIN_EXE_loop").into(),
        args: vec![DOCUMENT_READER_ARG.into()],
    });
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(HostExecutionEnv::new(dir.path())) as Arc<dyn ExecutionEnv>;
    (dir, env)
}

async fn read(env: &Arc<dyn ExecutionEnv>, path: &str) -> Result<String, String> {
    let tool = create_read_tool(Arc::clone(env));
    let r = (tool.execute)("r".into(), json!({ "path": path }), None, None).await?;
    match &r.content[0] {
        loop_ai::ToolResultContent::Text(t) => Ok(t.text.clone()),
        other => panic!("expected text, got {other:?}"),
    }
}

/// A PDF of `objects` (object `i + 1` is `objects[i]`), with a correct
/// cross-reference table.
fn pdf(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        writeln!(out, "{} 0 obj", i + 1).unwrap();
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    write!(out, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).unwrap();
    for offset in offsets {
        writeln!(out, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    )
    .unwrap();
    out
}

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream");
    out
}

/// One page whose content is `content` (object 4), with resources
/// `resources`, followed by `extra` objects (5 and up).
fn one_page(resources: &str, content: Vec<u8>, extra: Vec<Vec<u8>>) -> Vec<u8> {
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources {resources} /Contents 4 0 R >>").into_bytes(),
        content,
    ];
    objects.extend(extra);
    pdf(&objects)
}

#[tokio::test]
async fn a_pdf_form_that_draws_itself_fails_the_read_not_the_process() {
    let (dir, env) = setup();
    // Object 5 is a form XObject whose content draws object 5 again.
    let bytes = one_page(
        "<< /XObject << /X 5 0 R >> >>",
        stream("", b"/X Do"),
        vec![stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /XObject << /X 5 0 R >> >>",
            b"/X Do",
        )],
    );
    assert!(bytes.len() < 2048);
    std::fs::write(dir.path().join("loop.pdf"), &bytes).unwrap();
    let err = read(&env, "loop.pdf").await.unwrap_err();
    assert!(err.contains("PDF") || err.contains("safely"), "{err}");

    // A form without resources inherits the page's, which name it again.
    let bytes = one_page(
        "<< /XObject << /X 5 0 R >> >>",
        stream("", b"/X Do"),
        vec![stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 10 10]",
            b"/X Do",
        )],
    );
    std::fs::write(dir.path().join("inherit.pdf"), &bytes).unwrap();
    let err = read(&env, "inherit.pdf").await.unwrap_err();
    assert!(err.contains("PDF") || err.contains("safely"), "{err}");
}

#[tokio::test]
async fn a_flate_bomb_in_a_pdf_is_bounded() {
    let (dir, env) = setup();
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    let zeros = vec![b' '; 1024 * 1024];
    for _ in 0..400 {
        z.write_all(&zeros).unwrap();
    }
    let bomb = z.finish().unwrap();
    assert!(bomb.len() < 2 * 1024 * 1024, "{}", bomb.len());
    let bytes = one_page("<< >>", stream("/Filter /FlateDecode", &bomb), vec![]);
    std::fs::write(dir.path().join("bomb.pdf"), &bytes).unwrap();
    let started = Instant::now();
    let err = read(&env, "bomb.pdf").await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(40), "{err}");
    assert!(
        err.contains("too much") || err.contains("safely") || err.contains("PDF"),
        "{err}"
    );
}

fn zip(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in parts {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// An .xlsx whose one sheet holds `sheet_data` (the `<sheetData>` children).
fn xlsx(sheet_data: &str) -> Vec<u8> {
    let sheet = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{sheet_data}</sheetData></worksheet>"#
    );
    zip(&[
        ("[Content_Types].xml", br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#),
        ("_rels/.rels", br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#),
        ("xl/workbook.xml", br#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#),
        ("xl/_rels/workbook.xml.rels", br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#),
        ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
    ])
}

#[tokio::test]
async fn a_sparse_sheet_is_refused_fast() {
    let (dir, env) = setup();
    let bytes = xlsx(
        r#"<row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row><row r="1048576"><c r="XFD1048576" t="inlineStr"><is><t>z</t></is></c></row>"#,
    );
    std::fs::write(dir.path().join("sparse.xlsx"), &bytes).unwrap();
    let started = Instant::now();
    let err = read(&env, "sparse.xlsx").await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(err.contains("cells"), "{err}");

    // A small sheet still reads.
    let bytes = xlsx(r#"<row r="1"><c r="A1" t="inlineStr"><is><t>hello</t></is></c></row>"#);
    std::fs::write(dir.path().join("small.xlsx"), &bytes).unwrap();
    let text = read(&env, "small.xlsx").await.unwrap();
    assert!(text.contains("| 1 | hello |"), "{text}");
}

#[tokio::test]
async fn documents_round_trip_through_the_child() {
    let (dir, env) = setup();
    let write = create_write_tool(Arc::clone(&env));
    (write.execute)(
        "w".into(),
        json!({"path": "r.docx", "content": "# Hi\n\n- a\n- b"}),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(read(&env, "r.docx").await.unwrap(), "# Hi\n\n- a\n- b");

    // Overwriting diffs against the old text, read back by the child.
    let r = (write.execute)(
        "w2".into(),
        json!({"path": "r.docx", "content": "# Hi\n\n- a\n- c"}),
        None,
        None,
    )
    .await
    .unwrap();
    let diff = r.details["diff"].as_str().unwrap();
    assert!(diff.contains("-- b") && diff.contains("+- c"), "{diff}");
    assert!(diff.contains("formatting, images, comments"), "{diff}");

    std::fs::write(dir.path().join("junk.pptx"), b"not a zip").unwrap();
    let err = read(&env, "junk.pptx").await.unwrap_err();
    assert!(err.contains("zip"), "{err}");
}
