//! macOS Seatbelt execution and filesystem confinement.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::process::Command;

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
    network: bool,
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
            home.join(".ssh"),
            home.join(".aws"),
            home.join(".gnupg"),
            home.join(".config/gh"),
            home.join("Library/Keychains"),
            PathBuf::from("/Library/Keychains"),
            agent.join("auth.json"),
            agent.join("run"),
        ]
        .into_iter()
        .chain(runtime_paths.iter().cloned())
        {
            // Protect both the configured name and its resolved target.
            denied_paths.push(std::path::absolute(&path).map_err(io_error)?);
            denied_paths.push(resolve_path(&path)?);
        }
        if writes
            .iter()
            .chain(denied_paths.iter())
            .any(|p| p.to_str().is_none())
        {
            return Err(FileError::new(
                FileErrorCode::InvalidPath,
                "Seatbelt paths must be valid UTF-8",
            ));
        }
        Ok(Self {
            project,
            writes,
            denied: denied_paths,
            network,
        })
    }

    fn resolve(&self, path: &Path, write: bool) -> Result<PathBuf, FileError> {
        let absolute = self.project.join(path);
        let resolved = resolve_path(&absolute)?;
        if self
            .denied
            .iter()
            .any(|p| absolute.starts_with(p) || resolved.starts_with(p))
            || (write && !self.writes.iter().any(|p| resolved.starts_with(p)))
        {
            return Err(denied(&absolute));
        }
        Ok(resolved)
    }

    fn profile(&self) -> (String, Vec<String>) {
        let mut profile = String::from(
            r#"(version 1)
(deny default)
(allow process-exec process-fork)
(allow signal (target self))
(allow sysctl-read)
(allow file-read*)
(allow mach-lookup (global-name "com.apple.system.logger") (global-name "com.apple.system.notification_center") (global-name "com.apple.system.opendirectoryd.libinfo") (global-name "com.apple.system.configd") (global-name "com.apple.cfprefsd.daemon") (global-name "com.apple.cfprefsd.agent"))
(allow file-write* (literal (param "DEV_NULL")) (literal (param "DEV_TTY")))
"#,
        );
        let mut params = vec!["DEV_NULL=/dev/null".into(), "DEV_TTY=/dev/tty".into()];
        for (i, path) in self.writes.iter().enumerate() {
            profile.push_str(&format!(
                "(allow file-write* (subpath (param \"WRITE_{i}\")))\n"
            ));
            params.push(format!("WRITE_{i}={}", path.display()));
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
        // A pathname deny can otherwise be bypassed by renaming an allowed
        // ancestor (e.g. the whole agent dir) and reading auth.json there.
        let mut locked = std::collections::BTreeSet::new();
        for path in &self.denied {
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

    fn command(&self, program: &str, args: &[&str]) -> Command {
        let (profile, params) = self.profile();
        let mut cmd = Command::new("/usr/bin/sandbox-exec");
        cmd.arg("-p").arg(profile);
        for param in params {
            cmd.arg("-D").arg(param);
        }
        cmd.arg(program).args(args);
        cmd
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
            std::env::vars().filter(|(k, _)| safe_env_key(k)).collect()
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
        let mut cmd = self.policy.command("/bin/sh", &["-c", command]);
        cmd.current_dir(cwd).env_clear().envs(env);
        exec_command(cmd, options, true).await
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
        // Shell temp writes are allowed; file tools remain confined to explicit
        // project/allowWrite grants because their policy is cloned first.
        let fs = JailedHostFs::new(policy.clone());
        policy.writes.push(
            std::fs::canonicalize(std::env::temp_dir())
                .map_err(|e| SandboxError::Policy(e.to_string()))?,
        );
        Ok(Arc::new(SeatbeltSandbox {
            id: loop_ai::new_id(),
            env: Arc::new(SeatbeltEnv {
                fs,
                shell: JailedHostFs::new(policy),
            }),
            status: Mutex::new(SandboxStatus::Created),
        }))
    }
}

#[cfg(unix)]
struct SeatbeltEnv {
    fs: JailedHostFs,
    shell: JailedHostFs,
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
                .shell
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
        *self.status.lock() = SandboxStatus::Stopped;
        Ok(())
    }
    async fn destroy(&self) -> Result<(), SandboxError> {
        self.stop().await
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
    async fn exec(&self, c: &str, o: ShellExecOptions) -> Result<ShellOutput, ExecutionError> {
        self.shell.exec(c, o).await
    }
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
        let policy =
            SeatbeltPolicy::new(&project, root.path(), root.path(), &[], false, &[]).unwrap();
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
            let policy =
                SeatbeltPolicy::new(&project, root.path(), root.path(), &[], false, &[]).unwrap();
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
