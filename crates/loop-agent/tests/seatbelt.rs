#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use loop_agent::harness::sandbox::seatbelt::SeatbeltSandboxFactory;
use loop_agent::harness::types::{ExecutionErrorCode, ShellExecOptions, ShellOutput};
use loop_agent::harness::{
    create_edit_tool, create_glob_tool, create_grep_tool, create_ls_tool, create_read_tool,
    create_write_tool, ExecutionEnv, Sandbox, SandboxConfig, SandboxFactory,
};
use serde_json::{json, Value};

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
    agent: PathBuf,
    outside: PathBuf,
    sandbox: Arc<dyn Sandbox>,
}

impl Fixture {
    async fn new(network: bool) -> Self {
        // Outside-project tests must also be outside the system TMPDIR grant.
        let root = tempfile::Builder::new()
            .prefix(".seatbelt-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let project = root.path().join("project \" (allow default)");
        let home = root.path().join("home");
        let agent = root.path().join("actual-agent");
        let outside = root.path().join("outside");
        for path in [&project, &home, &agent, &outside] {
            std::fs::create_dir(path).unwrap();
        }
        let alias = root.path().join("agent-alias");
        std::os::unix::fs::symlink(&agent, &alias).unwrap();
        for name in [".ssh", ".aws", ".gnupg", ".config/gh", "Library/Keychains"] {
            std::fs::create_dir_all(home.join(name)).unwrap();
            std::fs::write(home.join(name).join("secret"), "sensitive-sentinel").unwrap();
        }
        std::fs::create_dir(agent.join("run")).unwrap();
        std::fs::write(agent.join("auth.json"), "sensitive-sentinel").unwrap();
        std::fs::write(agent.join("run/token"), "sensitive-sentinel").unwrap();
        let sandbox = SeatbeltSandboxFactory.create(SandboxConfig {
            workdir: project.clone(),
            options: json!({"homeDir": home, "agentDir": alias, "network": network, "allowWrite": [home, alias]}),
            ..Default::default()
        }).await.unwrap();
        sandbox.start().await.unwrap();
        Self {
            _root: root,
            project,
            home,
            agent,
            outside,
            sandbox,
        }
    }
    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.sandbox.env()
    }
    async fn sh(&self, command: &str) -> ShellOutput {
        self.env()
            .exec(
                command,
                ShellExecOptions {
                    timeout_ms: Some(10_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    }
}

fn q(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

async fn call(tool: loop_agent::AgentTool, args: Value) -> Result<String, String> {
    let result = (tool.execute)("test".into(), args, None, None).await?;
    Ok(result
        .content
        .iter()
        .filter_map(|v| match v {
            loop_ai::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[tokio::test]
async fn shell_and_file_tools_enforce_write_and_secret_boundaries() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let f = Fixture::new(false).await;
        let outside = f.outside.join("created");
        assert_ne!(
            f.sh(&format!("echo escape > {}", q(&outside)))
                .await
                .exit_code,
            0
        );
        assert!(call(
            create_write_tool(f.env()),
            json!({"path": outside, "content": "escape"})
        )
        .await
        .is_err());
        assert!(!outside.exists());
        call(
            create_write_tool(f.env()),
            json!({"path": "nested/ok", "content": "before"}),
        )
        .await
        .unwrap();
        call(
            create_edit_tool(f.env()),
            json!({"path": "nested/ok", "oldText": "before", "newText": "after"}),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(f.project.join("nested/ok")).unwrap(),
            "after"
        );
        for secret in [
            f.home.join(".ssh/secret"),
            f.home.join(".aws/secret"),
            f.home.join(".gnupg/secret"),
            f.home.join(".config/gh/secret"),
            f.home.join("Library/Keychains/secret"),
            f.agent.join("auth.json"),
            f.agent.join("run/token"),
        ] {
            let out = f.sh(&format!("cat {}", q(&secret))).await;
            assert_ne!(out.exit_code, 0, "{}: {out:?}", secret.display());
            assert!(!out.stdout.contains("sensitive-sentinel"));
            assert!(call(create_read_tool(f.env()), json!({"path": secret}))
                .await
                .is_err());
            assert!(call(
                create_write_tool(f.env()),
                json!({"path": secret, "content": "oops"})
            )
            .await
            .is_err());
            assert!(call(
                create_edit_tool(f.env()),
                json!({"path": secret, "oldText": "sensitive-sentinel", "newText": "oops"})
            )
            .await
            .is_err());
            assert!(call(
                create_grep_tool(f.env()),
                json!({"path": secret, "pattern": "."})
            )
            .await
            .is_err());
            assert!(call(
                create_glob_tool(f.env()),
                json!({"path": secret, "pattern": "*"})
            )
            .await
            .is_err());
            assert!(call(create_ls_tool(f.env()), json!({"path": secret}))
                .await
                .is_err());
        }
        std::os::unix::fs::symlink(&f.outside, f.project.join("escape")).unwrap();
        assert_ne!(f.sh("echo escape > escape/new").await.exit_code, 0);
        assert!(f
            .env()
            .write_file(Path::new("escape/new/deep/file"), b"escape")
            .await
            .is_err());
        std::os::unix::fs::symlink(&f.agent, f.project.join("secrets")).unwrap();
        assert_ne!(f.sh("cat secrets/auth.json").await.exit_code, 0);
        assert!(call(
            create_read_tool(f.env()),
            json!({"path": "secrets/auth.json"})
        )
        .await
        .is_err());
        for tool in [
            create_grep_tool(f.env()),
            create_glob_tool(f.env()),
            create_ls_tool(f.env()),
        ] {
            let text = call(
                tool,
                json!({"path": ".", "pattern": "sensitive-sentinel", "hidden": true}),
            )
            .await
            .unwrap();
            assert!(!text.contains("sensitive-sentinel"));
            assert!(!text.contains("auth.json"));
        }
        let link = f.home.join("linked-auth");
        let out = f
            .sh(&format!(
                "ln {} {} && cat {}",
                q(&f.agent.join("auth.json")),
                q(&link),
                q(&link)
            ))
            .await;
        assert_ne!(out.exit_code, 0, "{out:?}");
        assert!(!out.stdout.contains("sensitive-sentinel"));
        // An allowed parent must not let a command rename secrets out from
        // underneath a pathname deny rule.
        let renamed = f.home.join("moved-agent");
        let out = f
            .sh(&format!(
                "mv {} {} && cat {}/auth.json",
                q(&f.agent),
                q(&renamed),
                q(&renamed)
            ))
            .await;
        assert_ne!(out.exit_code, 0, "{out:?}");
        assert!(!out.stdout.contains("sensitive-sentinel"));
        let card = f.sandbox.info().format_box();
        for label in ["seatbelt", "partial", "off", "Writable paths"] {
            assert!(card.contains(label), "{card}");
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn network_and_runtime_unix_socket_are_denied() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let off = Fixture::new(false).await;
        let command = format!("/usr/bin/nc -z -w 1 127.0.0.1 {port}");
        assert_ne!(off.sh(&command).await.exit_code, 0);
        let on = Fixture::new(true).await;
        assert_eq!(on.sh(&command).await.exit_code, 0);
        let socket = on.agent.join("run/runtime.sock");
        let _unix = std::os::unix::net::UnixListener::bind(
            socket
                .strip_prefix(std::env::current_dir().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_ne!(
            on.sh(&format!(
                "/usr/bin/nc -w 1 -U {}",
                q(Path::new("../actual-agent/run/runtime.sock"))
            ))
            .await
            .exit_code,
            0
        );
        // Also cover a custom socket outside run/, as used by Runtime overrides.
        let socket = on.project.join("custom.sock");
        let _custom = std::os::unix::net::UnixListener::bind(
            socket
                .strip_prefix(std::env::current_dir().unwrap())
                .unwrap(),
        )
        .unwrap();
        let host_probe = tokio::process::Command::new("/usr/bin/nc")
            .current_dir(&on.project)
            .args(["-w", "1", "-U", "custom.sock"])
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(host_probe.status.success(), "{host_probe:?}");
        assert_ne!(
            on.sh(&format!(
                "/usr/bin/nc -w 1 -U {}",
                q(Path::new("custom.sock"))
            ))
            .await
            .exit_code,
            0
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn timeout_kills_descendants_and_environment_drops_secrets() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let f = Fixture::new(false).await;
        let options = ShellExecOptions {
            env: Some(
                [
                    ("OPENAI_API_KEY".into(), "sentinel".into()),
                    ("LOOP_RUNTIME_TOKEN".into(), "sentinel".into()),
                    ("PATH".into(), "/usr/bin:/bin".into()),
                ]
                .into(),
            ),
            timeout_ms: Some(10_000),
            ..Default::default()
        };
        let out = f.env().exec("env", options).await.unwrap();
        assert!(!out.stdout.contains("sentinel"));
        assert!(out.stdout.contains("PATH=/usr/bin:/bin"));
        let error = f
            .env()
            .exec(
                "sleep 60 & echo $! > child.pid; wait",
                ShellExecOptions {
                    timeout_ms: Some(500),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ExecutionErrorCode::TimedOut);
        let pid = std::fs::read_to_string(f.project.join("child.pid")).unwrap();
        // Wait for the outcome (reaping), rather than assert a wall-clock bound.
        loop {
            let output = tokio::process::Command::new("/bin/ps")
                .args(["-p", pid.trim(), "-o", "stat="])
                .output()
                .await
                .unwrap();
            if !output.status.success()
                || String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .starts_with('Z')
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn git_commit_and_cargo_build_work() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let f = Fixture::new(false).await;
        let out = f.sh("git init -q && git -c user.name=Sandbox -c user.email=sandbox@example.invalid -c commit.gpgsign=false commit --allow-empty -qm sandbox").await;
        assert_eq!(out.exit_code, 0, "{out:?}");
        std::fs::create_dir_all(f.project.join("src")).unwrap();
        std::fs::write(f.project.join("Cargo.toml"), "[package]\nname='seatbelt-smoke'\nversion='0.1.0'\nedition='2021'\n[workspace]\n").unwrap();
        std::fs::write(f.project.join("src/main.rs"), "fn main() {}\n").unwrap();
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".cargo"));
        let rustup_home = std::env::var_os("RUSTUP_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".rustup"));
        let sandbox = SeatbeltSandboxFactory.create(SandboxConfig { workdir: f.project.clone(), options: json!({"homeDir": home, "agentDir": f.agent, "allowWrite": [cargo_home, rustup_home]}), ..Default::default() }).await.unwrap();
        sandbox.start().await.unwrap();
        let out = sandbox.env().exec("cargo build --offline --target-dir target", ShellExecOptions { timeout_ms: Some(60_000), ..Default::default() }).await.unwrap();
        assert_eq!(out.exit_code, 0, "{out:?}");
        assert!(f.project.join("target/debug/seatbelt-smoke").is_file());
    }).await.unwrap();
}
