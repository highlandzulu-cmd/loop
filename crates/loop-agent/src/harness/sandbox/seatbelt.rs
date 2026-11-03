//! macOS Seatbelt execution and filesystem confinement.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::{Sandbox, SandboxConfig, SandboxError, SandboxFactory, SandboxInfo, SandboxStatus};
use crate::harness::env::exec_command;
use crate::harness::types::{
    ExecutionEnv, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo,
    FileSystem, Shell, ShellExecOptions, ShellOutput,
};

fn io_error(e: impl Into<std::io::Error>) -> FileError {
    let e = e.into();
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => FileErrorCode::NotFound,
        std::io::ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
        _ => FileErrorCode::Io,
    };
    FileError::new(code, e.to_string())
}

fn denied(path: &Path) -> FileError {
    FileError::new(
        FileErrorCode::PermissionDenied,
        format!("sandbox denies access to {}", path.display()),
    )
}

// Resolve existing ancestors too: canonicalize(path).unwrap_or(path) would let
// a symlink followed by a not-yet-created child escape the write jail.
fn resolve_path(path: &Path) -> Result<PathBuf, FileError> {
    let absolute = std::path::absolute(path).map_err(io_error)?;
    let mut out = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            part => {
                out.push(part.as_os_str());
                match std::fs::symlink_metadata(&out) {
                    Ok(_) => out = std::fs::canonicalize(&out).map_err(io_error)?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_error(e)),
                }
            }
        }
    }
    Ok(out)
}

/// Shared shell and file-tool policy. Paths are resolved before granting access.
#[derive(Clone)]
pub struct SeatbeltPolicy {
    project: PathBuf,
    writes: Vec<PathBuf>,
    denied: Vec<PathBuf>,
    write_denied: Vec<PathBuf>,
    database_dirs: Vec<PathBuf>,
    #[cfg(unix)]
    read_ids: std::collections::HashSet<(u64, u64)>,
    #[cfg(unix)]
    write_ids: std::collections::HashSet<(u64, u64)>,
    network: bool,
    temp_root: Option<PathBuf>,
}

impl SeatbeltPolicy {
    /// Build a policy with explicit home, agent directory and Runtime endpoints.
    pub fn new(
        project: &Path,
        home: &Path,
        agent: &Path,
        allow_write: &[PathBuf],
        network: bool,
        runtime_paths: &[PathBuf],
    ) -> Result<Self, FileError> {
        let project = std::fs::canonicalize(project).map_err(io_error)?;
        let mut writes = vec![project.clone()];
        for path in allow_write {
            writes.push(resolve_path(&project.join(path))?);
        }
        let mut denied_paths = vec![];
        for path in [
            home.join(".netrc"),
            home.join(".docker/config.json"),
            home.join(".kube/config"),
            home.join(".npmrc"),
            home.join(".pypirc"),
            home.join(".config/gcloud"),
            home.join("Library/Cookies"),
            home.join(".codex"),
            home.join(".claude"),
            home.join(".claude.json"),
            home.join(".gemini"),
            home.join(".qwen"),
            home.join(".copilot"),
            home.join(".config/github-copilot"),
            home.join(".cursor"),
            home.join(".continue"),
            home.join(".aider.conf.yml"),
            home.join(".config/opencode"),
            home.join(".local/share/opencode"),
            home.join(".config/goose"),
            home.join(".config/amp"),
            home.join(".local/share/amp"),
            home.join(".ssh"),
            home.join(".aws"),
            home.join(".gnupg"),
            home.join(".config/gh"),
            home.join("Library/Keychains"),
            PathBuf::from("/Library/Keychains"),
            agent.join("auth.json"),
            agent.join("run"),
            agent.join("mcp-auth"),
            agent.join("agents"),
            agent.join("history.json"),
            agent.join("history"),
            agent.join("logs"),
        ]
        .into_iter()
        .chain(runtime_paths.iter().cloned())
        {
            // Protect both the configured name and its resolved target.
            denied_paths.push(std::path::absolute(&path).map_err(io_error)?);
            denied_paths.push(resolve_path(&path)?);
        }
        let mut database_dirs = vec![
            std::path::absolute(agent).map_err(io_error)?,
            resolve_path(agent)?,
        ];
        // Session storage can live outside the agent directory. Protect its
        // SQLite files and sidecars too, without granting access to that root.
        if let Some(dir) =
            std::env::var_os("LOOP_CODING_AGENT_SESSION_DIR").filter(|v| !v.is_empty())
        {
            let dir = PathBuf::from(dir);
            database_dirs.push(std::path::absolute(&dir).map_err(io_error)?);
            database_dirs.push(resolve_path(&dir)?);
        }
        for dir in &database_dirs {
            for name in [
                "sessions.db",
                "sessions.db-wal",
                "sessions.db-shm",
                "events.db",
                "events.db-wal",
                "events.db-shm",
            ] {
                denied_paths.push(dir.join(name));
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("sessions.db") || name.starts_with("events.db") {
                        denied_paths.push(entry.path());
                    }
                }
            }
        }
        let mut write_denied = denied_paths.clone();
        for path in [
            agent.to_path_buf(),
            project.join(".loop"),
            project.join(".git/hooks"),
            project.join(".git/config"),
            project.join(".git/commondir"),
        ] {
            write_denied.push(std::path::absolute(&path).map_err(io_error)?);
            write_denied.push(resolve_path(&path)?);
        }
        // Git worktrees store their real metadata outside the worktree. Protect
        // the pointer itself and the shared hooks/config as well.
        let dot_git = project.join(".git");
        if dot_git.is_file() {
            write_denied.push(dot_git.clone());
            if let Some(dir) = std::fs::read_to_string(&dot_git)
                .map_err(io_error)?
                .trim()
                .strip_prefix("gitdir: ")
            {
                let dir = resolve_path(&project.join(dir))?;
                write_denied.push(dir.join("commondir"));
                write_denied.push(dir.join("config"));
                write_denied.push(dir.join("hooks"));
                if let Ok(common) = std::fs::read_to_string(dir.join("commondir")) {
                    let common = resolve_path(&dir.join(common.trim()))?;
                    write_denied.push(common.join("hooks"));
                    write_denied.push(common.join("config"));
                }
            }
        }
        if writes
            .iter()
            .chain(denied_paths.iter())
            .chain(write_denied.iter())
            .any(|p| p.to_str().is_none())
        {
            return Err(FileError::new(
                FileErrorCode::InvalidPath,
                "Seatbelt paths must be valid UTF-8",
            ));
        }
        #[cfg(unix)]
        let (read_ids, write_ids) = {
            use std::os::unix::fs::MetadataExt;
            let ids = |paths: &[PathBuf]| {
                paths
                    .iter()
                    .filter_map(|p| std::fs::metadata(p).ok())
                    .map(|m| (m.dev(), m.ino()))
                    .collect()
            };
            (ids(&denied_paths), ids(&write_denied))
        };
        Ok(Self {
            #[cfg(unix)]
            read_ids,
            #[cfg(unix)]
            write_ids,
            project,
            writes,
            denied: denied_paths,
            write_denied,
            database_dirs,
            network,
            temp_root: None,
        })
    }

    fn resolve(&self, path: &Path, write: bool) -> Result<PathBuf, FileError> {
        let absolute = self.project.join(path);
        let resolved = resolve_path(&absolute)?;
        if absolute.starts_with("/dev") || resolved.starts_with("/dev") {
            return Err(denied(&absolute));
        }
        if self.db_path(&absolute) || self.db_path(&resolved) {
            return Err(denied(&absolute));
        }
        if self
            .denied
            .iter()
            .any(|p| absolute.starts_with(p) || resolved.starts_with(p))
            || (write
                && (self
                    .write_denied
                    .iter()
                    .any(|p| absolute.starts_with(p) || resolved.starts_with(p))
                    || !self.writes.iter().any(|p| resolved.starts_with(p))))
        {
            return Err(denied(&absolute));
        }
        Ok(resolved)
    }

    fn db_path(&self, path: &Path) -> bool {
        self.database_dirs.iter().any(|root| {
            path.strip_prefix(root).ok().is_some_and(|relative| {
                relative.components().any(|c| {
                    let name = c.as_os_str().to_string_lossy();
                    name.starts_with("sessions.db") || name.starts_with("events.db")
                })
            })
        })
    }

    #[cfg(unix)]
    fn denied_identities(&self, write: bool) -> std::collections::HashSet<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        let mut paths = self.denied.clone();
        if write {
            paths.extend(self.write_denied.iter().cloned());
        }
        for agent in &self.database_dirs {
            if let Ok(entries) = std::fs::read_dir(agent) {
                paths.extend(
                    entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| self.db_path(p)),
                );
            }
        }
        let mut ids = self.read_ids.clone();
        if write {
            ids.extend(&self.write_ids);
        }
        ids.extend(
            paths
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok())
                .map(|m| (m.dev(), m.ino())),
        );
        ids
    }

    fn profile(&self) -> (String, Vec<String>) {
        let mut profile = String::from(
            r#"(version 1)
(deny default)
(deny system-mac-syscall (mac-policy-name "Sandbox") (mac-syscall-number 0))
(allow process-exec process-fork)
(allow signal (target self))
(allow sysctl-read)
(allow file-read*)
(allow mach-lookup (global-name "com.apple.system.logger") (global-name "com.apple.system.notification_center") (global-name "com.apple.system.opendirectoryd.libinfo") (global-name "com.apple.system.configd") (global-name "com.apple.cfprefsd.daemon") (global-name "com.apple.cfprefsd.agent"))
(allow file-write* (literal (param "DEV_NULL")))
"#,
        );
        let mut params = vec!["DEV_NULL=/dev/null".into()];
        for (i, path) in self.writes.iter().enumerate() {
            profile.push_str(&format!(
                "(allow file-write* (subpath (param \"WRITE_{i}\")))\n"
            ));
            params.push(format!("WRITE_{i}={}", path.display()));
        }
        if let Some(root) = &self.temp_root {
            profile.push_str("(deny file-write-mode file-write-owner file-write-flags (literal (param \"TMP_ROOT\")))\n");
            params.push(format!("TMP_ROOT={}", root.display()));
        }
        if self.network {
            profile.push_str(r#"(allow network*)
(allow mach-lookup (global-name "com.apple.mDNSResponder") (global-name "com.apple.trustd.agent") (global-name "com.apple.trustd"))
(deny network* (require-all (remote unix-socket) (require-not (literal (param "DNS_SOCKET")))))
"#);
            params.push("DNS_SOCKET=/private/var/run/mDNSResponder".into());
        } else {
            profile.push_str("(deny network* (remote unix-socket))\n");
        }
        // The only pathname Unix exception is the system-owned DNS resolver
        // when networking is on. Runtime and custom Unix sockets stay denied.
        for (i, path) in self.denied.iter().enumerate() {
            profile.push_str(&format!(
                "(deny file-read* file-write* (subpath (param \"DENY_{i}\")))\n"
            ));
            params.push(format!("DENY_{i}={}", path.display()));
        }
        for (i, path) in self.write_denied.iter().enumerate() {
            profile.push_str(&format!(
                "(deny file-write* (subpath (param \"WRITE_DENY_{i}\")))\n"
            ));
            params.push(format!("WRITE_DENY_{i}={}", path.display()));
        }
        for (i, path) in self.database_dirs.iter().enumerate() {
            profile.push_str(&format!("(deny file-read* file-write* (require-all (subpath (param \"AGENT_{i}\")) (regex #\"/(sessions|events)\\.db[^/]*(/|$)\")))\n"));
            params.push(format!("AGENT_{i}={}", path.display()));
        }
        // A pathname deny can otherwise be bypassed by renaming an allowed
        // ancestor (e.g. the whole agent dir) and reading auth.json there.
        let mut locked = std::collections::BTreeSet::new();
        for path in self.denied.iter().chain(self.write_denied.iter()) {
            for ancestor in path.ancestors().skip(1) {
                locked.insert(ancestor.to_path_buf());
            }
        }
        for (i, path) in locked.iter().enumerate() {
            profile.push_str(&format!(
                "(deny file-write-unlink (literal (param \"LOCK_{i}\")))\n"
            ));
            params.push(format!("LOCK_{i}={}", path.display()));
        }
        (profile, params)
    }

    fn command(
        &self,
        program: &str,
        args: &[&str],
    ) -> Result<(Command, tempfile::TempDir), ExecutionError> {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        builder.permissions(std::fs::Permissions::from_mode(0o700));
        let marker = builder
            .prefix("loop-command-")
            .tempdir_in("/tmp")
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
        let root = std::fs::canonicalize(marker.path())
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
        if root.to_str().is_none() {
            return Err(ExecutionError::new(
                ExecutionErrorCode::Io,
                "Seatbelt command identity path must be UTF-8",
            ));
        }
        for name in ["deny", "allow"] {
            std::fs::write(root.join(name), "command identity")
                .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
        }
        let (mut profile, mut params) = self.profile();
        profile.push_str("(deny file-read* (literal (param \"COMMAND_DENY\")))\n(deny file-write* (subpath (param \"COMMAND_ROOT\")))\n");
        params.push(format!("COMMAND_DENY={}", root.join("deny").display()));
        params.push(format!("COMMAND_ROOT={}", root.display()));
        let mut cmd = Command::new("/usr/bin/sandbox-exec");
        cmd.arg("-p").arg(profile);
        for param in params {
            cmd.arg("-D").arg(param);
        }
        cmd.arg(program).args(args);
        Ok((cmd, marker))
    }
}

fn safe_env_key(key: &str) -> bool {
    matches!(
        key,
        "PATH"
            | "HOME"
            | "USER"
            | "LOGNAME"
            | "SHELL"
            | "LANG"
            | "TERM"
            | "COLORTERM"
            | "TMPDIR"
            | "TZ"
            | "NO_COLOR"
            | "CLICOLOR"
            | "CARGO_HOME"
            | "RUSTUP_HOME"
            | "DEVELOPER_DIR"
            | "SDKROOT"
    ) || key.starts_with("LC_")
}

/// Host filesystem access with canonical policy checks and no-follow opens.
/// Descriptor-relative operations fail closed on a symlink replacement race.
pub struct JailedHostFs {
    policy: SeatbeltPolicy,
}

impl JailedHostFs {
    /// Bind file tools to the same policy as shell commands.
    pub fn new(policy: SeatbeltPolicy) -> Self {
        Self { policy }
    }
}

#[cfg(unix)]
impl JailedHostFs {
    fn open(
        &self,
        path: &Path,
        write: bool,
        flags: rustix::fs::OFlags,
        create_parents: bool,
    ) -> Result<std::fs::File, FileError> {
        use rustix::fs::{mkdirat, open, openat, Mode, OFlags};
        let path = self.policy.resolve(path, write)?;
        let denied_ids = self.policy.denied_identities(write);
        let mut fd = open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io_error)?;
        let parts: Vec<_> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(p) => Some(p),
                _ => None,
            })
            .collect();
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            let opts = if last {
                flags
            } else {
                OFlags::RDONLY | OFlags::DIRECTORY
            } | OFlags::NOFOLLOW
                | OFlags::CLOEXEC
                | OFlags::NONBLOCK;
            if create_parents && !last {
                match mkdirat(&fd, *part, Mode::from_raw_mode(0o755)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(io_error(e)),
                }
            }
            fd = openat(&fd, *part, opts, Mode::from_raw_mode(0o644)).map_err(io_error)?;
            let meta = std::fs::File::from(fd.try_clone().map_err(io_error)?)
                .metadata()
                .map_err(io_error)?;
            use std::os::unix::fs::MetadataExt;
            if denied_ids.contains(&(meta.dev(), meta.ino())) {
                return Err(denied(&path));
            }
        }
        let file = std::fs::File::from(fd);
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_dir() && !metadata.is_file() {
            return Err(denied(&path));
        }
        // Refuse aliases whose other names may be outside the jail.
        use std::os::unix::fs::MetadataExt;
        if metadata.is_file() && metadata.nlink() > 1 {
            return Err(denied(&path));
        }
        Ok(file)
    }

    fn info(&self, path: &Path) -> Result<FileInfo, FileError> {
        let file = self.open(path, false, rustix::fs::OFlags::RDONLY, false)?;
        let meta = file.metadata().map_err(io_error)?;
        Ok(FileInfo {
            path: self.policy.project.join(path),
            is_dir: meta.is_dir(),
            is_file: meta.is_file(),
            size: meta.len(),
        })
    }

    fn write(&self, path: &Path, data: &[u8], append: bool) -> Result<(), FileError> {
        use rustix::fs::OFlags;
        use std::io::Write;
        // Don't truncate until after the regular-file and link-count checks.
        let mut file = self.open(
            path,
            true,
            OFlags::WRONLY
                | OFlags::CREATE
                | if append {
                    OFlags::APPEND
                } else {
                    OFlags::empty()
                },
            true,
        )?;
        if !append {
            file.set_len(0).map_err(io_error)?;
        }
        file.write_all(data).map_err(io_error)
    }
}

#[cfg(unix)]
#[async_trait]
impl FileSystem for JailedHostFs {
    fn cwd(&self) -> &Path {
        &self.policy.project
    }
    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.policy.resolve(path, false)
    }
    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        base.join(child)
    }
    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        String::from_utf8(self.read_binary_file(path).await?)
            .map_err(|e| FileError::new(FileErrorCode::Io, e.to_string()))
    }
    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        use std::io::Read;
        let mut bytes = vec![];
        self.open(path, false, rustix::fs::OFlags::RDONLY, false)?
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        Ok(bytes)
    }
    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.write(path, data, false)
    }
    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.write(path, data, true)
    }
    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        self.info(path)
    }
    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        let file = self.open(
            path,
            false,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            false,
        )?;
        let dir = rustix::fs::Dir::read_from(&file).map_err(io_error)?;
        let mut entries = vec![];
        for entry in dir {
            let entry = entry.map_err(io_error)?;
            if entry.file_name() == c"." || entry.file_name() == c".." {
                continue;
            }
            use std::os::unix::ffi::OsStrExt;
            let child = self
                .policy
                .project
                .join(path)
                .join(std::ffi::OsStr::from_bytes(entry.file_name().to_bytes()));
            // Re-open every child under the policy. A denied entry is hidden.
            if let Ok(info) = self.info(&child) {
                entries.push(info);
            }
        }
        Ok(entries)
    }
    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        match self.info(path) {
            Ok(_) => Ok(true),
            Err(e) if e.code == FileErrorCode::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        // A sentinel final component lets open() create every directory relative
        // to pinned descriptors without creating or following the sentinel.
        let path = self.policy.resolve(path, true)?;
        let probe = path.join(format!(".loop-probe-{}", loop_ai::new_id()));
        match self.open(&probe, true, rustix::fs::OFlags::RDONLY, true) {
            Err(e) if e.code == FileErrorCode::NotFound => Ok(()),
            Ok(_) => Err(denied(&probe)),
            Err(e) => Err(e),
        }
    }
    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        let path = self.policy.resolve(path, true)?;
        let parent = path.parent().ok_or_else(|| denied(&path))?;
        let fd = self.open(
            parent,
            true,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            false,
        )?;
        let info = self.info(&path)?;
        let flags = if info.is_dir {
            rustix::fs::AtFlags::REMOVEDIR
        } else {
            rustix::fs::AtFlags::empty()
        };
        rustix::fs::unlinkat(&fd, path.file_name().ok_or_else(|| denied(&path))?, flags)
            .map_err(io_error)
    }
    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.info(path)?;
        self.policy.resolve(path, false)
    }
    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        if prefix.contains(['/', '\\']) || prefix == ".." {
            return Err(denied(Path::new(prefix)));
        }
        let path = self
            .policy
            .project
            .join(format!("{prefix}-{}", loop_ai::new_id()));
        self.create_dir(&path).await?;
        Ok(path)
    }
}

#[cfg(unix)]
#[async_trait]
impl Shell for JailedHostFs {
    async fn exec(
        &self,
        command: &str,
        options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError> {
        let cwd = self
            .policy
            .resolve(
                options.cwd.as_deref().unwrap_or(&self.policy.project),
                false,
            )
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
        let mut env: HashMap<String, String> = if options.inherit_env {
            std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .filter(|(k, _)| safe_env_key(k))
                .collect()
        } else {
            HashMap::new()
        };
        if let Some(overrides) = &options.env {
            env.extend(
                overrides
                    .iter()
                    .filter(|(k, _)| safe_env_key(k))
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        }
        let (mut cmd, marker) = self.policy.command("/bin/sh", &["-c", command])?;
        cmd.current_dir(cwd).env_clear().envs(env);
        let marker_path = std::fs::canonicalize(marker.path())
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?;
        let result = exec_command(cmd, options, true, Some(marker_path)).await;
        drop(marker);
        result
    }
}

/// Factory registered as the local sandbox on macOS.
pub struct SeatbeltSandboxFactory;

#[cfg(unix)]
#[async_trait]
impl SandboxFactory for SeatbeltSandboxFactory {
    fn kind(&self) -> &str {
        "local"
    }
    async fn create(&self, config: SandboxConfig) -> Result<Arc<dyn Sandbox>, SandboxError> {
        let path = |key: &str| {
            config
                .options
                .get(key)
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .ok_or_else(|| SandboxError::Policy(format!("Seatbelt requires {key}")))
        };
        let home = path("homeDir")?;
        let agent = path("agentDir")?;
        let allow = config
            .options
            .get("allowWrite")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(PathBuf::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let runtime = config
            .options
            .get("runtimePaths")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(PathBuf::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut policy = SeatbeltPolicy::new(
            &config.workdir,
            &home,
            &agent,
            &allow,
            config
                .options
                .get("network")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            &runtime,
        )
        .map_err(|e| SandboxError::Policy(e.to_string()))?;
        // Use a trusted parent, never the ambient TMPDIR. tempfile creates
        // this directory exclusively; verify its owner, mode and resolved path.
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::Builder::new()
            .prefix("loop-seatbelt-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")
            .map_err(|e| SandboxError::Policy(e.to_string()))?;
        let temp_path =
            std::fs::canonicalize(temp.path()).map_err(|e| SandboxError::Policy(e.to_string()))?;
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(&temp_path)
            .map_err(|e| SandboxError::Policy(e.to_string()))?;
        // SAFETY: geteuid has no arguments or preconditions.
        if !meta.is_dir()
            || meta.mode() & 0o777 != 0o700
            || meta.uid() != unsafe { libc::geteuid() }
            || temp_path.to_str().is_none()
        {
            return Err(SandboxError::Policy(
                "Seatbelt requires a private, user-owned UTF-8 temporary directory".into(),
            ));
        }
        let canary_read = temp_path.join("denied-read");
        let canary_write = temp_path.join("denied-write");
        std::fs::write(&canary_read, "Seatbelt preflight canary")
            .map_err(|e| SandboxError::Policy(e.to_string()))?;
        policy.denied.push(canary_read.clone());
        policy.write_denied.push(canary_write.clone());
        let fs = JailedHostFs::new(policy.clone());
        policy.writes.push(temp_path.clone());
        policy.temp_root = Some(temp_path.clone());
        Ok(Arc::new(SeatbeltSandbox {
            id: loop_ai::new_id(),
            env: Arc::new(SeatbeltEnv {
                fs,
                shell: JailedHostFs::new(policy),
                temp: Mutex::new(Some(temp)),
                temp_path,
                canary_read,
                canary_write,
                stopped: CancellationToken::new(),
                active: Mutex::new(0),
                cleanup_error: Mutex::new(None),
                idle: tokio::sync::Notify::new(),
            }),
            status: Mutex::new(SandboxStatus::Created),
        }))
    }
}

#[cfg(unix)]
struct SeatbeltEnv {
    fs: JailedHostFs,
    shell: JailedHostFs,
    temp: Mutex<Option<tempfile::TempDir>>,
    temp_path: PathBuf,
    canary_read: PathBuf,
    canary_write: PathBuf,
    stopped: CancellationToken,
    active: Mutex<usize>,
    cleanup_error: Mutex<Option<String>>,
    idle: tokio::sync::Notify,
}

#[cfg(unix)]
struct Flight<'a>(&'a SeatbeltEnv);

#[cfg(unix)]
impl Drop for Flight<'_> {
    fn drop(&mut self) {
        *self.0.active.lock() -= 1;
        self.0.idle.notify_one();
    }
}

#[cfg(unix)]
struct SeatbeltSandbox {
    id: String,
    env: Arc<SeatbeltEnv>,
    status: Mutex<SandboxStatus>,
}

#[cfg(unix)]
#[async_trait]
impl Sandbox for SeatbeltSandbox {
    fn kind(&self) -> &str {
        "local"
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn status(&self) -> SandboxStatus {
        *self.status.lock()
    }
    fn info(&self) -> SandboxInfo {
        SandboxInfo::enabled(
            "local",
            vec![
                ("Status".into(), self.status().to_string()),
                ("Runtime".into(), "seatbelt".into()),
                ("Isolation".into(), "partial".into()),
                (
                    "Shell network".into(),
                    if self.env.shell.policy.network {
                        "on"
                    } else {
                        "off"
                    }
                    .into(),
                ),
                (
                    "Writable paths".into(),
                    self.env
                        .shell
                        .policy
                        .writes
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            ],
        )
    }
    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.env.clone()
    }
    async fn start(&self) -> Result<(), SandboxError> {
        *self.status.lock() = SandboxStatus::Starting;
        let result = async {
            if !Path::new("/usr/bin/sandbox-exec").is_file() {
                return Err(SandboxError::StartFailed(
                    "/usr/bin/sandbox-exec is missing; Seatbelt is unavailable".into(),
                ));
            }
            let out = self
                .env
                .exec(
                    "/usr/bin/true",
                    ShellExecOptions {
                        timeout_ms: Some(5_000),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| SandboxError::StartFailed(e.to_string()))?;
            if out.exit_code != 0 {
                return Err(SandboxError::StartFailed(format!(
                    "Seatbelt preflight failed: {}",
                    out.stderr.trim()
                )));
            }
            let script = format!("if /bin/cat {} >/dev/null 2>&1; then exit 71; fi; if /bin/sh -c 'echo canary > \"$1\"' canary {} 2>/dev/null; then exit 72; fi", shell_quote(&self.env.canary_read), shell_quote(&self.env.canary_write));
            let canaries = self.env.exec(&script, ShellExecOptions { timeout_ms: Some(5_000), ..Default::default() }).await.map_err(|e| SandboxError::StartFailed(e.to_string()))?;
            if canaries.exit_code != 0 || self.env.canary_write.exists() {
                return Err(SandboxError::StartFailed("Seatbelt preflight failed: read/write denial canary did not hold".into()));
            }
            Ok(())
        }
        .await;
        *self.status.lock() = if result.is_ok() {
            SandboxStatus::Ready
        } else {
            SandboxStatus::Failed
        };
        result
    }
    async fn stop(&self) -> Result<(), SandboxError> {
        self.env.stopped.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let notified = self.env.idle.notified();
                if *self.env.active.lock() == 0 {
                    break;
                }
                notified.await;
            }
        })
        .await
        .map_err(|_| SandboxError::Other("Seatbelt commands did not finish cleanup".into()))?;
        if let Some(error) = self.env.cleanup_error.lock().clone() {
            *self.status.lock() = SandboxStatus::Failed;
            return Err(SandboxError::Other(error));
        }
        *self.status.lock() = SandboxStatus::Stopped;
        Ok(())
    }
    async fn destroy(&self) -> Result<(), SandboxError> {
        self.stop().await?;
        if let Some(temp) = self.env.temp.lock().take() {
            temp.close()
                .map_err(|e| SandboxError::Other(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(unix)]
#[async_trait]
impl FileSystem for SeatbeltEnv {
    fn cwd(&self) -> &Path {
        self.fs.cwd()
    }
    fn absolute_path(&self, p: &Path) -> Result<PathBuf, FileError> {
        self.fs.absolute_path(p)
    }
    fn join_path(&self, b: &Path, p: &Path) -> PathBuf {
        b.join(p)
    }
    async fn read_text_file(&self, p: &Path) -> Result<String, FileError> {
        self.fs.read_text_file(p).await
    }
    async fn read_binary_file(&self, p: &Path) -> Result<Vec<u8>, FileError> {
        self.fs.read_binary_file(p).await
    }
    async fn write_file(&self, p: &Path, d: &[u8]) -> Result<(), FileError> {
        self.fs.write_file(p, d).await
    }
    async fn append_file(&self, p: &Path, d: &[u8]) -> Result<(), FileError> {
        self.fs.append_file(p, d).await
    }
    async fn file_info(&self, p: &Path) -> Result<FileInfo, FileError> {
        self.fs.file_info(p).await
    }
    async fn list_dir(&self, p: &Path) -> Result<Vec<FileInfo>, FileError> {
        self.fs.list_dir(p).await
    }
    async fn exists(&self, p: &Path) -> Result<bool, FileError> {
        self.fs.exists(p).await
    }
    async fn create_dir(&self, p: &Path) -> Result<(), FileError> {
        self.fs.create_dir(p).await
    }
    async fn remove(&self, p: &Path) -> Result<(), FileError> {
        self.fs.remove(p).await
    }
    async fn canonical_path(&self, p: &Path) -> Result<PathBuf, FileError> {
        self.fs.canonical_path(p).await
    }
    async fn create_temp_dir(&self, p: &str) -> Result<PathBuf, FileError> {
        self.fs.create_temp_dir(p).await
    }
}

#[cfg(unix)]
#[async_trait]
impl Shell for SeatbeltEnv {
    async fn exec(&self, c: &str, mut o: ShellExecOptions) -> Result<ShellOutput, ExecutionError> {
        {
            let mut active = self.active.lock();
            if self.stopped.is_cancelled() {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Aborted,
                    "Seatbelt sandbox stopped",
                ));
            }
            *active += 1;
        }
        let _flight = Flight(self);
        o.env.get_or_insert_default().insert(
            "TMPDIR".into(),
            self.temp_path.to_string_lossy().into_owned(),
        );
        let caller_cancel = o.cancel.take().unwrap_or_default();
        let cancel = CancellationToken::new();
        o.cancel = Some(cancel.clone());
        let command = self.shell.exec(c, o);
        tokio::pin!(command);
        let result = tokio::select! {
            biased;
            _ = self.stopped.cancelled() => { cancel.cancel(); command.await }
            _ = caller_cancel.cancelled() => { cancel.cancel(); command.await }
            result = &mut command => result,
        };
        if self.stopped.is_cancelled() {
            if let Err(error) = &result {
                if error.code == ExecutionErrorCode::Io {
                    *self.cleanup_error.lock() = Some(error.to_string());
                }
            }
        }
        result
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_parameters_not_profile_source() {
        let root = tempfile::tempdir().unwrap();
        let tricky = root.path().join(if cfg!(windows) {
            "quote (allow default)"
        } else {
            "quote\" (allow default)"
        });
        std::fs::create_dir(&tricky).unwrap();
        let policy =
            SeatbeltPolicy::new(&tricky, root.path(), root.path(), &[], false, &[]).unwrap();
        let (profile, params) = policy.profile();
        assert!(!profile.contains("quote"));
        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert!(params
            .iter()
            .any(|p| p == &format!("WRITE_0={}", tricky.canonicalize().unwrap().display())));
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn resolves_missing_children_but_not_symlink_escapes() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let policy = SeatbeltPolicy::new(
            &project,
            root.path(),
            &root.path().join("agent"),
            &[],
            false,
            &[],
        )
        .unwrap();
        assert!(policy.resolve(&project.join("new/deep/file"), true).is_ok());
        assert!(policy.resolve(&root.path().join("outside"), true).is_err());
        assert!(policy.resolve(&project.join("../outside"), true).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path(), project.join("escape")).unwrap();
            assert!(policy
                .resolve(&project.join("escape/new/file"), true)
                .is_err());
        }
    }

    #[test]
    fn denied_paths_override_broad_write_grants() {
        let root = tempfile::tempdir().unwrap();
        let policy = SeatbeltPolicy::new(
            root.path(),
            root.path(),
            root.path(),
            &[root.path().to_path_buf()],
            true,
            &[],
        )
        .unwrap();
        for name in [
            ".ssh/key",
            ".aws/config",
            ".gnupg/key",
            ".config/gh/hosts.yml",
            "Library/Keychains/login.keychain-db",
            "auth.json",
            "run/runtime.sock",
        ] {
            assert!(
                policy.resolve(&root.path().join(name), false).is_err(),
                "{name}"
            );
            assert!(
                policy.resolve(&root.path().join(name), true).is_err(),
                "{name}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_symlink_replacement_cannot_redirect_writes() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let root = tempfile::tempdir().unwrap();
            let project = root.path().join("project");
            let inside = project.join("inside");
            let outside = root.path().join("outside");
            std::fs::create_dir_all(&inside).unwrap();
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("file"), "unchanged").unwrap();
            let policy = SeatbeltPolicy::new(
                &project,
                root.path(),
                &root.path().join("agent"),
                &[],
                false,
                &[],
            )
            .unwrap();
            let fs = JailedHostFs::new(policy);
            let alias = project.join("alias");
            let swap = project.join("swap");
            std::os::unix::fs::symlink(&inside, &alias).unwrap();
            let writer = std::thread::spawn(move || {
                for i in 0..2_000 {
                    let _ = std::fs::remove_file(&swap);
                    std::os::unix::fs::symlink(if i % 2 == 0 { &inside } else { &outside }, &swap)
                        .unwrap();
                    std::fs::rename(&swap, &alias).unwrap();
                }
            });
            for _ in 0..2_000 {
                let _ = fs.write_file(Path::new("alias/file"), b"changed").await;
                tokio::task::yield_now().await;
            }
            tokio::task::spawn_blocking(move || writer.join().unwrap())
                .await
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(root.path().join("outside/file")).unwrap(),
                "unchanged"
            );
        })
        .await
        .unwrap();
    }

    #[test]
    fn child_environment_is_allowlisted() {
        for name in [
            "OPENAI_API_KEY",
            "LOOP_RUNTIME_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "GH_TOKEN",
            "DYLD_INSERT_LIBRARIES",
            "BASH_ENV",
            "CARGO_BUILD_RUSTC_WRAPPER",
            "CUSTOM_PASSWORD",
        ] {
            assert!(!safe_env_key(name), "{name}");
        }
        for name in ["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TMPDIR"] {
            assert!(safe_env_key(name), "{name}");
        }
    }
}
