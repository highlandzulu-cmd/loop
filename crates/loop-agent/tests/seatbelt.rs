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
            options: json!({"homeDir": home, "agentDir": alias, "network": network, "allowWrite": [home, alias], "runtimePaths": [root.path().join("runtime-fallback")]}),
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
        let socket = off.project.join("custom.sock");
        let _off_socket = std::os::unix::net::UnixListener::bind(
            socket
                .strip_prefix(std::env::current_dir().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_ne!(off.sh("/usr/bin/nc -w 1 -U custom.sock").await.exit_code, 0);
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
        let init = tokio::process::Command::new("git").args(["init", "-q"]).current_dir(&f.project).output().await.unwrap();
        assert!(init.status.success());
        let out = f.sh("git -c user.name=Sandbox -c user.email=sandbox@example.invalid -c commit.gpgsign=false commit --allow-empty -qm sandbox").await;
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

#[tokio::test]
async fn configuration_credentials_and_descriptor_aliases_are_confined() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let f = Fixture::new(false).await;
        for path in [
            f.project.join(".loop/settings.json"),
            f.project.join(".git/hooks/pre-commit"),
            f.project.join(".git/config"),
            f.agent.join("settings.json"),
            f.agent.join("trust.json"),
            f.agent.join("bin/evil"),
        ] {
            assert!(
                f.env().write_file(&path, b"evil").await.is_err(),
                "{}",
                path.display()
            );
            assert_ne!(
                f.sh(&format!(
                    "mkdir -p {} && echo evil > {}",
                    q(path.parent().unwrap()),
                    q(&path)
                ))
                .await
                .exit_code,
                0,
                "{}",
                path.display()
            );
        }
        for name in [
            ".netrc",
            ".docker/config.json",
            ".kube/config",
            ".npmrc",
            ".pypirc",
            ".config/gcloud/token",
            "Library/Cookies/Cookies.binarycookies",
            ".codex/auth.json",
            ".claude/.credentials.json",
            ".gemini/oauth_creds.json",
        ] {
            let path = f.home.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "private-token").unwrap();
            assert!(f.env().read_text_file(&path).await.is_err(), "{name}");
            assert_ne!(
                f.sh(&format!("cat {}", q(&path))).await.exit_code,
                0,
                "{name}"
            );
        }
        for name in [
            "mcp-auth/token",
            "agents/chatgpt/home/auth.json",
            "sessions.db",
            "sessions.db-wal",
            "events.db-shm",
            "history.json",
            "logs/runtime.log",
        ] {
            let path = f.agent.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "private-token").unwrap();
            assert!(f.env().read_text_file(&path).await.is_err(), "{name}");
            assert_ne!(
                f.sh(&format!("cat {}", q(&path))).await.exit_code,
                0,
                "{name}"
            );
        }
        let fallback = f._root.path().join("runtime-fallback/token");
        std::fs::create_dir_all(fallback.parent().unwrap()).unwrap();
        std::fs::write(&fallback, "runtime-token").unwrap();
        assert!(f.env().read_text_file(&fallback).await.is_err());
        assert_ne!(f.sh(&format!("cat {}", q(&fallback))).await.exit_code, 0);
        use std::os::fd::AsRawFd;
        let secret = std::fs::File::open(f.agent.join("auth.json")).unwrap();
        assert!(f
            .env()
            .read_text_file(Path::new(&format!("/dev/fd/{}", secret.as_raw_fd())))
            .await
            .is_err());
        assert!(f
            .env()
            .read_binary_file(Path::new("/dev/null"))
            .await
            .is_err());
        let renamed = f.project.join("renamed-auth");
        std::fs::rename(f.agent.join("auth.json"), &renamed).unwrap();
        assert!(f.env().read_text_file(&renamed).await.is_err());
        assert!(f.env().write_file(&renamed, b"evil").await.is_err());
        std::fs::rename(&renamed, f.agent.join("auth.json")).unwrap();
        let outside = f.outside.join("ordinary");
        std::fs::write(&outside, "outside").unwrap();
        let alias = f.project.join("hardlink");
        assert_ne!(
            f.sh(&format!("ln {} {}", q(&outside), q(&alias)))
                .await
                .exit_code,
            0
        );
        std::fs::hard_link(&outside, &alias).unwrap();
        assert!(f.env().read_text_file(&alias).await.is_err());
        assert!(f.env().write_file(&alias, b"evil").await.is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "outside");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn temporary_directory_is_private_and_cannot_be_overridden() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let f = Fixture::new(false).await;
        let other = Fixture::new(false).await;
        let out = f
            .env()
            .exec(
                "printf '%s' \"$TMPDIR\"",
                ShellExecOptions {
                    env: Some([("TMPDIR".into(), "/".into())].into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let path = PathBuf::from(out.stdout);
        assert_ne!(path, Path::new("/"));
        assert_ne!(path, std::env::temp_dir());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_ne!(f.sh("chmod 777 \"$TMPDIR\"").await.exit_code, 0);
        assert_eq!(f.sh("echo ok > \"$TMPDIR/test\"").await.exit_code, 0);
        assert_ne!(
            other
                .sh(&format!("echo evil > {}", q(&path.join("other"))))
                .await
                .exit_code,
            0
        );
        assert_ne!(f.sh("echo injected >/dev/tty").await.exit_code, 0);
        f.sandbox.destroy().await.unwrap();
        assert!(!path.exists());
        assert!(f.env().exec("true", Default::default()).await.is_err());
    })
    .await
    .unwrap();
}

async fn gone(pid: &str) {
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
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn detached_descendants_are_killed_on_return_timeout_and_destroy() {
    tokio::time::timeout(Duration::from_secs(45), async {
        for finish in ["return", "timeout", "cancel", "drop", "stop", "destroy"] {
            let f = Fixture::new(false).await;
            // Clear the environment too: cleanup cannot rely on a removable
            // environment variable or on the child's process group/session.
            let script = r#"/usr/bin/perl -MPOSIX -e 'setsid() >= 0 or die; my $pid = fork(); defined $pid or die; exit if $pid; setsid() >= 0 or die; exec "/usr/bin/env", "-i", "/bin/sh", "-c", "echo $$ > escaped.pid; exec /bin/sleep 60"' & while [ ! -s escaped.pid ]; do sleep 0.01; done"#;
            let script = if finish == "return" { script.to_owned() } else { format!("{script}; /bin/sleep 60") };
            let env = f.env();
            let cancel = tokio_util::sync::CancellationToken::new();
            let trigger = cancel.clone();
            let command = tokio::spawn(async move { env.exec(&script, ShellExecOptions { timeout_ms: Some(700), cancel: Some(cancel), ..Default::default() }).await });
            let pid = loop {
                if let Ok(pid) = std::fs::read_to_string(f.project.join("escaped.pid")) {
                    if pid.trim().parse::<u32>().is_ok_and(|p| p > 1) { break pid; }
                }
                tokio::task::yield_now().await;
            };
            if finish == "cancel" { trigger.cancel(); }
            if finish == "drop" { command.abort(); }
            if finish == "stop" { f.sandbox.stop().await.unwrap(); }
            if finish == "destroy" { f.sandbox.destroy().await.unwrap(); }
            let result = command.await;
            if finish == "timeout" { assert_eq!(result.unwrap().unwrap_err().code, ExecutionErrorCode::TimedOut); }
            else if finish == "drop" { assert!(result.unwrap_err().is_cancelled()); }
            else if finish == "return" { assert_eq!(result.unwrap().unwrap().exit_code, 0); }
            else { assert_eq!(result.unwrap().unwrap_err().code, ExecutionErrorCode::Aborted); }
            gone(&pid).await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn ambient_tmpdir_cannot_widen_writes() {
    tokio::time::timeout(Duration::from_secs(30), async {
        if std::env::var_os("LOOP_SEATBELT_TMPDIR_TEST").is_some() {
            let f = Fixture::new(false).await;
            let actual = f.sh("printf '%s' \"$TMPDIR\"").await.stdout;
            assert!(actual.starts_with("/private/tmp/loop-seatbelt-"));
            assert_ne!(
                f.sh(&format!("echo escape > {}", q(&f.outside.join("escape"))))
                    .await
                    .exit_code,
                0
            );
            return;
        }
        use std::os::unix::ffi::OsStringExt;
        for value in [
            std::ffi::OsString::from("/"),
            std::ffi::OsString::from_vec(b"/tmp/non-utf8-\xff".to_vec()),
        ] {
            let child = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "ambient_tmpdir_cannot_widen_writes",
                    "--nocapture",
                ])
                .env("LOOP_SEATBELT_TMPDIR_TEST", "1")
                .env("TMPDIR", value)
                .output()
                .await
                .unwrap();
            assert!(
                child.status.success(),
                "{}",
                String::from_utf8_lossy(&child.stderr)
            );
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn nested_profiles_cannot_hide_cleanup_identity() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let f = Fixture::new(false).await;
        let out = f
            .sh("/usr/bin/sandbox-exec -p '(version 1)(allow default)' /usr/bin/true")
            .await;
        assert_ne!(
            out.exit_code, 0,
            "a nested profile must not replace the cleanup marker"
        );
        assert!(out.stderr.contains("sandbox_apply"), "{out:?}");
    })
    .await
    .unwrap();
}
