//! Discovery and detached startup for the default interactive Runtime client.
use loop_protocol::AgentBackend;
use loop_wire::RemoteBackend;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const CONNECT_WAIT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Unix(PathBuf),
    WebSocket(String),
}
impl Target {
    pub fn parse(value: &str) -> Self {
        if value.starts_with("ws://") || value.starts_with("wss://") {
            Self::WebSocket(value.into())
        } else {
            Self::Unix(value.into())
        }
    }
    pub fn label(&self) -> String {
        match self {
            Self::Unix(p) => p.display().to_string(),
            Self::WebSocket(s) => s.clone(),
        }
    }
}

/// Current discovery has one location. Keep selection independent of that
/// inventory so additional registries can supply the same reachable targets.
pub async fn discover(agent_dir: &Path) -> anyhow::Result<Vec<Target>> {
    let socket = crate::config::paths::runtime_socket_path(agent_dir)?;
    Ok(if tokio::net::UnixStream::connect(&socket).await.is_ok() {
        vec![Target::Unix(socket)]
    } else {
        vec![]
    })
}

pub fn select(
    targets: &[Target],
    mut pick: impl FnMut(&[Target]) -> anyhow::Result<usize>,
) -> anyhow::Result<Option<Target>> {
    match targets {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        many => many
            .get(pick(many)?)
            .cloned()
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("invalid Runtime selection")),
    }
}

pub fn pick(targets: &[Target]) -> anyhow::Result<usize> {
    use std::io::Write;
    eprintln!("Choose a Runtime:");
    for (i, target) in targets.iter().enumerate() {
        eprintln!("  {}. {}", i + 1, target.label());
    }
    eprint!("> ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    answer
        .trim()
        .parse::<usize>()?
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("invalid Runtime selection"))
}

pub async fn connect(
    target: &Target,
    token_file: Option<&Path>,
    host: Arc<dyn loop_protocol::HostServices>,
) -> anyhow::Result<Arc<RemoteBackend>> {
    tokio::time::timeout(CONNECT_WAIT, async {
        let transport = match target {
            Target::Unix(socket) => {
                loop_wire::socket::unix(tokio::net::UnixStream::connect(socket).await?)
            }
            Target::WebSocket(url) => {
                let token = match token_file {
                    Some(path) => loop_wire::token::read(path)?,
                    None => std::env::var("LOOP_RUNTIME_TOKEN")
                        .ok()
                        .filter(|v| !v.trim().is_empty())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "WebSocket connections require LOOP_RUNTIME_TOKEN or --token-file"
                            )
                        })?,
                };
                loop_wire::socket::websocket_connect(url, token.trim()).await?
            }
        };
        Ok::<_, anyhow::Error>(crate::surface_client::connect(transport, host).await?)
    })
    .await
    .map_err(|_| anyhow::anyhow!("Runtime connection timed out"))?
}

/// `setsid` plus closed stdin separates the owner from this terminal's lifetime.
pub fn spawn(
    executable: &Path,
    agent_dir: &Path,
    cwd: &Path,
    socket: &Path,
) -> anyhow::Result<std::process::Child> {
    spawn_owner(executable, agent_dir, cwd, socket, true)
}

fn spawn_owner(
    executable: &Path,
    agent_dir: &Path,
    cwd: &Path,
    socket: &Path,
    auto_started: bool,
) -> anyhow::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    crate::config::paths::ensure_agent_dirs(agent_dir)?;
    let (log, _) = crate::config::paths::open_runtime_log(agent_dir)?;
    let mut command = std::process::Command::new(executable);
    command
        .args(["runtime", "start", "--socket"])
        .arg(socket)
        .arg("--cwd")
        .arg(cwd)
        .env("LOOP_CODING_AGENT_DIR", agent_dir)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if auto_started {
        command.arg("--auto-started");
    }
    // SAFETY: setsid is async-signal-safe and uses no Rust locks after fork.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command.spawn()?)
}

pub async fn start_and_connect(
    executable: &Path,
    agent_dir: &Path,
    cwd: &Path,
    socket: &Path,
    host: Arc<dyn loop_protocol::HostServices>,
) -> anyhow::Result<Arc<RemoteBackend>> {
    start_owner(executable, agent_dir, cwd, socket, host, true).await
}

async fn start_owner(
    executable: &Path,
    agent_dir: &Path,
    cwd: &Path,
    socket: &Path,
    host: Arc<dyn loop_protocol::HostServices>,
    auto_started: bool,
) -> anyhow::Result<Arc<RemoteBackend>> {
    let mut child = spawn_owner(executable, agent_dir, cwd, socket, auto_started)?;
    let result = tokio::time::timeout(CONNECT_WAIT, async {
        loop {
            if let Ok(remote) = connect(&Target::Unix(socket.into()), None, host.clone()).await {
                break Ok(remote);
            }
            if let Some(status) = child.try_wait()? {
                // Another launcher may have won the Runtime lock. Its socket
                // may still be starting; wait for it under the same deadline.
                if status.success() {
                    anyhow::bail!("Runtime exited before accepting connections");
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Runtime did not start; see logs/runtime.log"))
    .and_then(|result| result);
    // Reap a child owned by this client without making it a Tokio child that
    // could be killed when the Surface exits.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    result
}

/// An explicit restart creates a detached owner without an idle deadline.
pub async fn restart(socket: Option<PathBuf>, cwd: &Path) -> anyhow::Result<()> {
    let agent_dir = crate::config::paths::get_agent_dir();
    let socket = match socket {
        Some(socket) => socket,
        None => crate::config::paths::runtime_socket_path(&agent_dir)?,
    };
    crate::runtime_cmd::stop(Some(socket.clone())).await?;
    let remote = start_owner(
        &std::env::current_exe()?,
        &agent_dir,
        cwd,
        &socket,
        Arc::new(crate::surface_client::RemoteHost::default()),
        false,
    )
    .await?;
    eprintln!("Runtime restarted (pid {}).", remote.status().await?.pid);
    Ok(())
}

/// Check before opening or attaching any Session. Never stop a foreign host.
pub async fn compatible(remote: &RemoteBackend) -> anyhow::Result<bool> {
    if remote.server().version == env!("CARGO_PKG_VERSION") {
        return Ok(true);
    }
    if remote.status().await?.loaded == 0 {
        return Ok(false);
    }
    anyhow::bail!(
        "Runtime is v{}, this loop is v{}; run `loop runtime restart` when its Sessions are done",
        remote.server().version,
        env!("CARGO_PKG_VERSION")
    )
}

pub async fn launch(
    explicit: Option<&str>,
    token_file: Option<&Path>,
    agent_dir: &Path,
    cwd: &Path,
    host: Arc<dyn loop_protocol::HostServices>,
) -> anyhow::Result<Arc<RemoteBackend>> {
    let target = match explicit {
        Some(value) => Some(Target::parse(value)),
        None => select(&discover(agent_dir).await?, pick)?,
    };
    let executable = std::env::current_exe()?;
    let remote = match &target {
        Some(target) => connect(target, token_file, host.clone()).await?,
        None => {
            start_and_connect(
                &executable,
                agent_dir,
                cwd,
                &crate::config::paths::runtime_socket_path(agent_dir)?,
                host.clone(),
            )
            .await?
        }
    };
    if compatible(&remote).await? {
        if !remote.capabilities().session_environment {
            anyhow::bail!("Runtime does not accept per-Session environments");
        }
        return Ok(remote);
    }
    let socket = match target {
        Some(Target::WebSocket(_)) => anyhow::bail!(
            "Runtime is v{}, this loop is v{}; restart the Runtime on its host",
            remote.server().version,
            env!("CARGO_PKG_VERSION")
        ),
        Some(Target::Unix(socket)) => socket,
        None => crate::config::paths::runtime_socket_path(agent_dir)?,
    };
    // Only restart the local owner belonging to this agent directory.
    if socket != crate::config::paths::runtime_socket_path(agent_dir)? {
        anyhow::bail!("Runtime version differs; restart it on its host");
    }
    let stopped = if remote.capabilities().runtime_stop_empty {
        remote.stop_empty().await
    } else {
        remote.stop().await
    };
    match stopped {
        Ok(()) => {}
        Err(loop_protocol::BackendError::Failed(info))
            if info.category == loop_protocol::ErrorCategory::Network => {}
        Err(error) => return Err(error.into()),
    }
    tokio::time::timeout(CONNECT_WAIT, async {
        while tokio::net::UnixStream::connect(&socket).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Runtime did not stop"))?;
    eprintln!(
        "loop: restarted an idle Runtime to use v{}",
        env!("CARGO_PKG_VERSION")
    );
    start_and_connect(&executable, agent_dir, cwd, &socket, host).await
}

/// Resolve an attach request before creating anything. Prefix ambiguity is an
/// error; `--continue` chooses the latest root Session in this cwd.
pub fn resume_session(
    infos: &[loop_protocol::rpc::SessionInfo],
    cwd: &Path,
    id: Option<&str>,
    latest: bool,
    mut picker: impl FnMut(&[loop_protocol::rpc::SessionInfo]) -> anyhow::Result<usize>,
) -> anyhow::Result<Option<loop_protocol::SessionRef>> {
    if id.is_none() && !latest {
        return Ok(None);
    }
    let mut choices: Vec<_> = infos
        .iter()
        .filter(|info| {
            if let Some(id) = id.filter(|id| !id.is_empty()) {
                info.r#ref.native_id.starts_with(id)
            } else {
                info.cwd == cwd && info.parent.is_none()
            }
        })
        .cloned()
        .collect();
    choices.sort_by_key(|info| std::cmp::Reverse(info.last_active_ms));
    if choices.is_empty() {
        anyhow::bail!("no matching Session");
    }
    if latest {
        return Ok(Some(choices[0].r#ref.clone()));
    }
    if id.is_some_and(|id| !id.is_empty()) {
        if choices.len() != 1 {
            anyhow::bail!("Session id is ambiguous; use a longer id");
        }
        return Ok(Some(choices[0].r#ref.clone()));
    }
    choices
        .get(picker(&choices)?)
        .map(|info| Some(info.r#ref.clone()))
        .ok_or_else(|| anyhow::anyhow!("invalid Session selection"))
}

pub async fn open(
    remote: Arc<RemoteBackend>,
    host: Arc<crate::surface_client::RemoteHost>,
    opts: &crate::BootstrapOpts,
    resume: Option<&str>,
    latest: bool,
) -> anyhow::Result<crate::SurfaceClient> {
    let resumed = resume.is_some() || latest;
    let session = if resumed {
        resume_session(
            &remote.list().await?,
            &opts.cwd,
            resume,
            latest,
            |choices| {
                use std::io::Write;
                eprintln!("Choose a Session:");
                for (i, info) in choices.iter().enumerate() {
                    eprintln!(
                        "  {}. {} · {}",
                        i + 1,
                        info.r#ref.native_id,
                        info.name
                            .as_deref()
                            .or(info.summary.first_prompt.as_deref())
                            .unwrap_or("(empty)")
                    );
                }
                eprint!("> ");
                std::io::stderr().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                answer
                    .trim()
                    .parse::<usize>()?
                    .checked_sub(1)
                    .ok_or_else(|| anyhow::anyhow!("invalid Session selection"))
            },
        )?
        .unwrap()
    } else {
        if !remote.capabilities().session_environment {
            anyhow::bail!("Runtime does not accept per-Session environments");
        }
        remote
            .new_session(loop_protocol::SessionSpec {
                cwd: Some(opts.cwd.clone()),
                env: Some(std::env::vars().collect()),
                name: None,
                options: Some(loop_protocol::SessionOptions {
                    provider: opts.provider.clone(),
                    model: opts.model.clone(),
                    system_prompt: opts.system_prompt.clone(),
                    append_system_prompt: opts.append_system_prompt.clone(),
                    no_context_files: opts.no_context_files,
                    no_web: opts.no_web,
                    permission_mode: opts.permission_mode.clone(),
                }),
            })
            .await?
    };
    crate::SurfaceClient::connected(remote, host, opts, session, resumed).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn picker_uses_the_discovered_inventory() {
        let one = Target::Unix("/tmp/one.sock".into());
        let two = Target::Unix("/tmp/two.sock".into());
        assert_eq!(
            select(std::slice::from_ref(&one), |_| panic!("no picker for one")).unwrap(),
            Some(one.clone())
        );
        assert_eq!(
            select(&[one, two.clone()], |targets| {
                assert_eq!(targets.len(), 2);
                Ok(1)
            })
            .unwrap(),
            Some(two)
        );
        assert!(select(&[], |_| panic!("no picker for none"))
            .unwrap()
            .is_none());
    }
    #[test]
    fn resume_latest_prefix_and_picker_choose_existing_sessions() {
        use loop_protocol::rpc::SessionInfo;
        let make = |id: &str, time| SessionInfo {
            r#ref: loop_protocol::SessionRef {
                instance: "native".into(),
                key: id.into(),
                native_id: id.into(),
            },
            cwd: "/work".into(),
            name: None,
            parent: None,
            live: true,
            busy: false,
            created_ms: 0,
            last_active_ms: time,
            summary: Default::default(),
        };
        let infos = vec![make("abc-one", 1), make("abc-two", 2)];
        assert_eq!(
            resume_session(&infos, Path::new("/work"), None, true, |_| panic!())
                .unwrap()
                .unwrap()
                .native_id,
            "abc-two"
        );
        assert_eq!(
            resume_session(
                &infos,
                Path::new("/work"),
                Some("abc-o"),
                false,
                |_| panic!()
            )
            .unwrap()
            .unwrap()
            .native_id,
            "abc-one"
        );
        assert!(
            resume_session(&infos, Path::new("/work"), Some("abc"), false, |_| panic!()).is_err()
        );
        assert_eq!(
            resume_session(&infos, Path::new("/work"), Some(""), false, |_| Ok(1))
                .unwrap()
                .unwrap()
                .native_id,
            "abc-one"
        );
        assert!(
            resume_session(&infos, Path::new("/work"), None, false, |_| panic!())
                .unwrap()
                .is_none()
        );
    }
}
