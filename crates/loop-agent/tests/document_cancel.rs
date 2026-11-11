//! A cancelled read or write stops the document reader child at once.
//! This binary's reader is a child that never answers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use loop_agent::harness::{
    create_read_tool, create_write_tool, set_document_reader, DocumentReader, ExecutionEnv,
    HostExecutionEnv,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn setup() -> (tempfile::TempDir, Arc<dyn ExecutionEnv>) {
    set_document_reader(DocumentReader::Child {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "sleep 60".into(), "sh".into()],
    });
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("slow.docx"), b"PK whatever").unwrap();
    let env = Arc::new(HostExecutionEnv::new(dir.path())) as Arc<dyn ExecutionEnv>;
    (dir, env)
}

fn cancel_soon() -> CancellationToken {
    let token = CancellationToken::new();
    let later = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        later.cancel();
    });
    token
}

#[tokio::test]
async fn a_cancelled_read_returns_at_once() {
    let (_dir, env) = setup();
    let read = create_read_tool(env);
    let started = Instant::now();
    let err = (read.execute)(
        "r".into(),
        json!({"path": "slow.docx"}),
        Some(cancel_soon()),
        None,
    )
    .await
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10), "{err}");
    assert!(err.contains("cancelled"), "{err}");
}

#[tokio::test]
async fn a_cancelled_write_leaves_the_file_alone() {
    let (dir, env) = setup();
    let write = create_write_tool(env);
    let started = Instant::now();
    let err = (write.execute)(
        "w".into(),
        json!({"path": "slow.docx", "content": "# New"}),
        Some(cancel_soon()),
        None,
    )
    .await
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10), "{err}");
    assert!(err.contains("cancelled"), "{err}");
    assert_eq!(
        std::fs::read(dir.path().join("slow.docx")).unwrap(),
        b"PK whatever"
    );
}
