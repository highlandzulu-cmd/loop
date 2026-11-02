#![cfg(unix)]
//! `loop runtime start|stop|status` end to end: spawn the built binary with an isolated agent dir.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::{Child, Command};

fn serve(agent: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loop"));
    cmd.args(["runtime", "start"])
        .args(args)
        .env("LOOP_CODING_AGENT_DIR", agent)
        .env("LOOP_CODING_AGENT_SESSION_DIR", agent.join("sessions"))
        .env_remove("LOOP_DEBUG")
        .env_remove("RUST_LOG")
        .current_dir(agent)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

async fn wait_for(path: &Path, child: &mut Child) {
    let wait = async {
        while !path.exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("loop runtime start exited early: {status}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .expect("socket never appeared");
}

fn signal(child: &Child, name: &str) {
    assert!(std::process::Command::new("kill")
        .args([name, &child.id().unwrap().to_string()])
        .status()
        .unwrap()
        .success());
}

fn agent_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("lsv")
        .tempdir_in("/tmp")
        .unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    (dir, agent)
}

#[tokio::test]
async fn serve_starts_without_stdin_then_cleans_up_on_sigterm() {
    let (_dir, agent) = agent_dir();
    let socket = agent.join("run/runtime.sock");
    let mut child = serve(&agent, &[]).spawn().unwrap();
    wait_for(&socket, &mut child).await;
    assert!(agent.join("run/runtime.pid").exists());
    assert!(agent.join("events.db").exists());
    signal(&child, "-TERM");
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "{status}");
    assert!(!socket.exists(), "socket removed");
    assert!(!agent.join("run/runtime.pid").exists(), "pid file removed");
    let log = std::fs::read_to_string(agent.join("logs/runtime.log")).unwrap();
    assert!(log.contains("Runtime logging enabled"));
}

#[tokio::test]
async fn second_serve_with_same_agent_dir_refuses() {
    let (_dir, agent) = agent_dir();
    let socket = agent.join("run/runtime.sock");
    let mut first = serve(&agent, &[]).spawn().unwrap();
    wait_for(&socket, &mut first).await;
    let output = tokio::time::timeout(Duration::from_secs(30), serve(&agent, &[]).output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already running"), "{stderr}");
    // The first Runtime is untouched.
    assert!(socket.exists());
    signal(&first, "-INT");
    let status = tokio::time::timeout(Duration::from_secs(30), first.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn explicit_socket_and_invalid_permission_mode() {
    let (_dir, agent) = agent_dir();
    let output = serve(&agent, &["--permission-mode", "yolo"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("permission-mode"));

    let socket = agent.join("custom.sock");
    let socket_arg = socket.to_str().unwrap();
    let mut child = serve(&agent, &["--socket", socket_arg]).spawn().unwrap();
    wait_for(&socket, &mut child).await;
    signal(&child, "-TERM");
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert!(!socket.exists());
}

#[tokio::test]
async fn a_client_connects_through_remote_backend() {
    use loop_protocol::{AgentBackend, HostServices};
    let (_dir, agent) = agent_dir();
    let socket = agent.join("run/runtime.sock");
    let mut child = serve(&agent, &[]).spawn().unwrap();
    wait_for(&socket, &mut child).await;
    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let backend = loop_wire::RemoteBackend::connect(
        loop_wire::socket::unix(stream),
        loop_protocol::rpc::PeerInfo {
            name: "test".into(),
            version: "0".into(),
        },
        std::sync::Arc::new(NoHost) as std::sync::Arc<dyn HostServices>,
    )
    .await
    .unwrap();
    assert!(backend.capabilities().multi_session);
    assert_eq!(backend.server().name, "loop");
    drop(backend);
    signal(&child, "-TERM");
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}

struct NoHost;
#[async_trait::async_trait]
impl loop_protocol::HostServices for NoHost {
    async fn interact(
        &self,
        _: loop_protocol::InteractionRequest,
    ) -> loop_protocol::InteractionOutcome {
        loop_protocol::InteractionOutcome::Rejected {
            reason: "test host".into(),
        }
    }
}

#[tokio::test]
async fn shared_session_database_is_owned_by_one_runtime() {
    let (_dir, first_agent) = agent_dir();
    let second_agent = first_agent.parent().unwrap().join("agent2");
    std::fs::create_dir_all(&second_agent).unwrap();
    let shared = first_agent.join("shared-sessions");
    let socket = first_agent.join("run/runtime.sock");
    let mut first = serve(&first_agent, &[])
        .env("LOOP_CODING_AGENT_SESSION_DIR", &shared)
        .spawn()
        .unwrap();
    wait_for(&socket, &mut first).await;
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        serve(&second_agent, &[])
            .env("LOOP_CODING_AGENT_SESSION_DIR", &shared)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Session database"), "{stderr}");
    assert!(socket.exists(), "first Runtime untouched");
    signal(&first, "-TERM");
    first.wait().await.unwrap();
}

fn loop_cmd(agent: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loop"));
    cmd.args(args)
        .env("LOOP_CODING_AGENT_DIR", agent)
        .env("LOOP_CODING_AGENT_SESSION_DIR", agent.join("sessions"))
        .env_remove("LOOP_DEBUG")
        .env_remove("RUST_LOG")
        .current_dir(agent)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

async fn run(agent: &Path, args: &[&str]) -> std::process::Output {
    tokio::time::timeout(Duration::from_secs(30), loop_cmd(agent, args).output())
        .await
        .expect("command hung")
        .unwrap()
}

#[tokio::test]
async fn status_reports_a_running_runtime_and_exits_1_without_one() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_dir, agent) = agent_dir();
        let output = run(&agent, &["runtime", "status"]).await;
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("No Runtime running"));
        let output = run(&agent, &["runtime", "status", "--json"]).await;
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());

        let socket = agent.join("run/runtime.sock");
        let mut child = serve(&agent, &["--max-sessions", "3"]).spawn().unwrap();
        wait_for(&socket, &mut child).await;
        let output = run(&agent, &["runtime", "status", "--json"]).await;
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let status: loop_protocol::rpc::RuntimeStatus =
            serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status.pid, child.id().unwrap());
        assert_eq!(status.socket, socket);
        assert_eq!((status.loaded, status.busy), (0, 0));
        assert_eq!((status.max_sessions, status.max_active_turns), (3, 8));
        let output = run(&agent, &["runtime", "status"]).await;
        assert_eq!(output.status.code(), Some(0));
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("0 loaded of 3"), "{text}");
        signal(&child, "-TERM");
        child.wait().await.unwrap();
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn stop_shuts_the_runtime_down_through_its_socket() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_dir, agent) = agent_dir();
        let socket = agent.join("run/runtime.sock");
        let mut child = serve(&agent, &[]).spawn().unwrap();
        wait_for(&socket, &mut child).await;
        let output = run(&agent, &["runtime", "stop"]).await;
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("stopped"));
        assert!(!socket.exists(), "socket removed");
        let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "{status}");
        assert!(!agent.join("run/runtime.pid").exists(), "pid file removed");
        // Stopping again finds nothing and touches nothing.
        let output = run(&agent, &["runtime", "stop"]).await;
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("No Runtime running"));
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn stop_never_signals_the_pid_in_a_stale_pid_file() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (_dir, agent) = agent_dir();
        let mut bystander = Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        std::fs::create_dir_all(agent.join("run")).unwrap();
        std::fs::write(
            agent.join("run/runtime.pid"),
            bystander.id().unwrap().to_string(),
        )
        .unwrap();
        let output = run(&agent, &["runtime", "stop"]).await;
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("No Runtime running"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            bystander.try_wait().unwrap().is_none(),
            "the pid in a stale pid file was signalled"
        );
        bystander.kill().await.unwrap();
    })
    .await
    .expect("test timed out");
}
