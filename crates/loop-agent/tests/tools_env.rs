//! HostExecutionEnv + built-in tools tests.

use std::sync::Arc;

use loop_agent::harness::{
    create_bash_tool, create_edit_tool, create_read_tool, create_write_tool, HostExecutionEnv,
};
use serde_json::json;

#[tokio::test]
async fn write_read_edit_bash() {
    let dir = tempfile::tempdir().unwrap();
    let env =
        Arc::new(HostExecutionEnv::new(dir.path())) as Arc<dyn loop_agent::harness::ExecutionEnv>;

    let write = create_write_tool(Arc::clone(&env));
    (write.execute)(
        "1".into(),
        json!({"path": "f.txt", "content": "hello world"}),
        None,
        None,
    )
    .await
    .unwrap();

    let read = create_read_tool(Arc::clone(&env));
    let r = (read.execute)("2".into(), json!({"path": "f.txt"}), None, None)
        .await
        .unwrap();
    assert!(
        matches!(&r.content[0], loop_ai::ToolResultContent::Text(t) if t.text.contains("hello"))
    );

    let edit = create_edit_tool(Arc::clone(&env));
    (edit.execute)(
        "3".into(),
        json!({"path": "f.txt", "oldText": "hello", "newText": "hi"}),
        None,
        None,
    )
    .await
    .unwrap();

    let bash = create_bash_tool(env);
    let out = (bash.execute)("4".into(), json!({"command": "echo ok"}), None, None)
        .await
        .unwrap();
    assert!(
        matches!(&out.content[0], loop_ai::ToolResultContent::Text(t) if t.text.contains("ok"))
    );
}

/// A read-only file in the working directory: the tool errors say how to
/// make it writable, so the agent does not edit a copy elsewhere.
#[cfg(unix)]
#[tokio::test]
async fn permission_denied_errors_hint_at_chmod() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ro.txt");
    std::fs::write(&file, "hello").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new().append(true).open(&file).is_ok() {
        // Root writes read-only files: nothing to test.
        return;
    }
    let env =
        Arc::new(HostExecutionEnv::new(dir.path())) as Arc<dyn loop_agent::harness::ExecutionEnv>;

    let write = create_write_tool(Arc::clone(&env));
    let err = (write.execute)(
        "1".into(),
        json!({"path": "ro.txt", "content": "x"}),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.contains("chmod u+w"), "{err}");

    let edit = create_edit_tool(Arc::clone(&env));
    let err = (edit.execute)(
        "2".into(),
        json!({"path": "ro.txt", "oldText": "hello", "newText": "hi"}),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.contains("chmod u+w"), "{err}");

    let bash = create_bash_tool(env);
    let out = (bash.execute)(
        "3".into(),
        json!({"command": "echo x > ro.txt"}),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        matches!(&out.content[0], loop_ai::ToolResultContent::Text(t) if t.text.contains("chmod u+w")),
        "{:?}",
        out.content
    );
}
