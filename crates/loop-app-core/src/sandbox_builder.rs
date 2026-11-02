//! One place that turns sandbox settings or `/sandbox local` flags into a running sandbox.
//!
//! Both `/sandbox local` and the Session factory go through [`SandboxBuilder`]. The
//! builder picks the platform's sandbox from a [`SandboxRegistry`]: on Linux that is
//! the Podman sandbox ([`KrunSandboxFactory`]); macOS has none yet. A new platform
//! sandbox only needs a factory registered in [`platform_registry`] under
//! [`LOCAL_SANDBOX_KIND`].
//!
//! Isolation and runtime are optional. A value the user set (a flag, or a key in
//! `settings.json`) is explicit; a missing value inherits the platform default. An
//! explicit Linux-only value on macOS is an error; an inherited default never is.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use loop_agent::harness::{
    KrunIsolation, LocalSandboxRuntime, Sandbox, SandboxConfig, SandboxRegistry,
};
use serde_json::json;

use crate::config::settings::SandboxSettings;

/// Registry kind for the platform's local sandbox.
pub const LOCAL_SANDBOX_KIND: &str = "local";

/// Operating system family, which decides sandbox defaults and what is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPlatform {
    /// Linux: Podman containers.
    Linux,
    /// macOS: Seatbelt (not available yet).
    MacOs,
    /// Anything else: no local sandbox.
    Other,
}

impl SandboxPlatform {
    /// The platform this binary was built for.
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Other
        }
    }

    /// Isolation used when the user did not choose one.
    pub fn default_isolation(self) -> KrunIsolation {
        match self {
            Self::MacOs => KrunIsolation::Partial,
            Self::Linux | Self::Other => KrunIsolation::Full,
        }
    }

    /// Container runtime used when the user did not choose one; `None` where the
    /// sandbox does not use containers.
    pub fn default_runtime(self) -> Option<LocalSandboxRuntime> {
        match self {
            Self::Linux => Some(LocalSandboxRuntime::Runc),
            Self::MacOs | Self::Other => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Linux => "Linux",
            Self::MacOs => "macOS",
            Self::Other => "this platform",
        }
    }
}

/// Isolation and runtime chosen with `/sandbox local` flags. `None` means not given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalSandboxFlags {
    /// `--full` or `--partial`.
    pub isolation: Option<KrunIsolation>,
    /// `--crun`, `--runc`, `--runsc` or `--krun`.
    pub runtime: Option<LocalSandboxRuntime>,
}

/// Parse flags after `/sandbox local` (`--full|--partial`, `--crun|--runc|--runsc|--krun`).
///
/// Positional `full`/`partial`/`runc`/… are accepted too. A flag left out stays `None`,
/// so the platform default applies.
pub fn parse_local_sandbox_flags(args: &[&str]) -> Result<LocalSandboxFlags, String> {
    let mut flags = LocalSandboxFlags::default();
    for tok in args {
        let raw = tok.trim();
        if raw.is_empty() {
            continue;
        }
        let key = raw.trim_start_matches('-').to_ascii_lowercase();
        match key.as_str() {
            "full" | "partial" => {
                if flags.isolation.is_some() {
                    return Err("specify only one of --full / --partial".into());
                }
                flags.isolation = KrunIsolation::parse(&key);
            }
            "crun" | "runc" | "runsc" | "gvisor" | "krun" => {
                if flags.runtime.is_some() {
                    return Err("specify only one runtime (--crun|--runc|--runsc|--krun)".into());
                }
                flags.runtime = LocalSandboxRuntime::parse(&key);
            }
            other => {
                return Err(format!(
                    "unknown sandbox option '{other}' (use --full|--partial and --crun|--runc|--runsc|--krun)"
                ));
            }
        }
    }
    Ok(flags)
}

/// What the user asked for, before platform defaults are applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxRequest {
    /// Explicit isolation, if any.
    pub isolation: Option<KrunIsolation>,
    /// Explicit container runtime, if any.
    pub runtime: Option<LocalSandboxRuntime>,
    /// Allow network access from sandboxed commands.
    pub network: bool,
    /// Extra writable paths (`~` expanded; relative paths are under the workspace).
    pub allow_write: Vec<PathBuf>,
}

impl SandboxRequest {
    /// Read the request from settings. An unknown isolation or runtime is an error.
    pub fn from_settings(settings: &SandboxSettings) -> Result<Self, SandboxBuildError> {
        let isolation = settings
            .isolation
            .as_deref()
            .map(|value| {
                KrunIsolation::parse(value).ok_or_else(|| SandboxBuildError::InvalidSetting {
                    key: "sandbox.isolation",
                    value: value.into(),
                    expected: "full or partial",
                })
            })
            .transpose()?;
        let runtime = settings
            .runtime
            .as_deref()
            .map(|value| {
                LocalSandboxRuntime::parse(value).ok_or_else(|| SandboxBuildError::InvalidSetting {
                    key: "sandbox.runtime",
                    value: value.into(),
                    expected: "crun, runc, runsc or krun",
                })
            })
            .transpose()?;
        // Older builds saved this exact pair into every settings file, whether or
        // not the user chose it. Read it as unset so it inherits the platform
        // default (the same values on Linux; no error on macOS).
        let (isolation, runtime) = if isolation == Some(KrunIsolation::Full)
            && runtime == Some(LocalSandboxRuntime::Runc)
        {
            (None, None)
        } else {
            (isolation, runtime)
        };
        Ok(Self {
            isolation,
            runtime,
            network: settings.network(),
            allow_write: settings
                .allow_write
                .iter()
                .map(|p| expand_home(p))
                .collect(),
        })
    }

    /// Request for `/sandbox local`: isolation and runtime come from the flags only
    /// (a flag left out falls back to the platform default, not to the saved
    /// setting), while network and writable paths come from settings. Saved
    /// isolation and runtime are not read, so a bad saved value never blocks the
    /// command that replaces it.
    pub fn for_flags(settings: &SandboxSettings, flags: LocalSandboxFlags) -> Self {
        Self {
            isolation: flags.isolation,
            runtime: flags.runtime,
            network: settings.network(),
            allow_write: settings
                .allow_write
                .iter()
                .map(|p| expand_home(p))
                .collect(),
        }
    }
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// A request with platform defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPlan {
    /// Isolation to use.
    pub isolation: KrunIsolation,
    /// Container runtime, where the platform sandbox uses containers.
    pub runtime: Option<LocalSandboxRuntime>,
    /// Allow network access.
    pub network: bool,
    /// Extra writable paths.
    pub allow_write: Vec<PathBuf>,
}

impl SandboxPlan {
    /// Flags as shown to people, e.g. `--full --runc` or `--partial`.
    pub fn label(&self) -> String {
        match self.runtime {
            Some(runtime) => format!("--{} --{}", self.isolation.as_str(), runtime.as_str()),
            None => format!("--{}", self.isolation.as_str()),
        }
    }

    /// Factory config for `workdir`. Backends read the keys they support:
    /// the Podman sandbox reads `isolation` and `runtime`, and ignores
    /// `network` and `allowWrite` for now.
    pub fn config(&self, workdir: &Path) -> SandboxConfig {
        let allow_write: Vec<String> = self
            .allow_write
            .iter()
            .map(|p| workdir.join(p).display().to_string())
            .collect();
        let mut options = json!({
            "isolation": self.isolation.as_str(),
            "network": self.network,
            "allowWrite": allow_write,
        });
        if let Some(runtime) = self.runtime {
            options["runtime"] = json!(runtime.as_str());
        }
        SandboxConfig {
            workdir: workdir.to_path_buf(),
            options,
            labels: Default::default(),
        }
    }
}

/// Why a sandbox could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBuildError {
    /// A settings value is not recognised.
    InvalidSetting {
        /// Settings key.
        key: &'static str,
        /// The value found.
        value: String,
        /// The accepted values.
        expected: &'static str,
    },
    /// The user chose a Linux-only container runtime on another platform.
    LinuxOnlyRuntime {
        /// Platform in use.
        platform: SandboxPlatform,
        /// The runtime chosen.
        runtime: LocalSandboxRuntime,
    },
    /// The user chose full isolation where it is not available.
    FullIsolationUnavailable {
        /// Platform in use.
        platform: SandboxPlatform,
    },
    /// No sandbox exists for this platform yet.
    Unavailable {
        /// Platform in use.
        platform: SandboxPlatform,
    },
    /// The sandbox could not be created or started.
    Start(String),
}

impl fmt::Display for SandboxBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSetting {
                key,
                value,
                expected,
            } => write!(f, "{key} is \"{value}\" in settings; use {expected}"),
            Self::LinuxOnlyRuntime { platform, runtime } => write!(
                f,
                "--{rt} is a Linux container runtime and is not available on {os}. \
                 Leave out --{rt}, and remove sandbox.runtime from settings.json if it is set there.",
                rt = runtime.as_str(),
                os = platform.name(),
            ),
            Self::FullIsolationUnavailable { platform } => write!(
                f,
                "--full isolation is not available on {os} yet. Use --partial or leave it out, \
                 and remove sandbox.isolation from settings.json if it is set to \"full\" there.",
                os = platform.name(),
            ),
            Self::Unavailable { platform } => write!(
                f,
                "the local sandbox is not available on {} yet; tools run on the host (/sandbox off)",
                platform.name()
            ),
            Self::Start(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for SandboxBuildError {}

/// Registry with the sandbox factories this platform supports.
pub fn platform_registry() -> SandboxRegistry {
    let registry = SandboxRegistry::new();
    #[cfg(target_os = "linux")]
    registry.register(Arc::new(loop_agent::harness::KrunSandboxFactory));
    // macOS: the Seatbelt factory registers here under LOCAL_SANDBOX_KIND.
    registry
}

/// Builds the platform's local sandbox from a [`SandboxRequest`].
pub struct SandboxBuilder {
    platform: SandboxPlatform,
    registry: SandboxRegistry,
}

impl SandboxBuilder {
    /// Builder for `platform` over `registry` (tests inject both).
    pub fn new(platform: SandboxPlatform, registry: SandboxRegistry) -> Self {
        Self { platform, registry }
    }

    /// Builder for the platform this binary runs on.
    pub fn for_current_platform() -> Self {
        Self::new(SandboxPlatform::current(), platform_registry())
    }

    /// Platform this builder targets.
    pub fn platform(&self) -> SandboxPlatform {
        self.platform
    }

    /// Apply platform defaults, rejecting explicit values the platform cannot honour.
    pub fn resolve(&self, request: &SandboxRequest) -> Result<SandboxPlan, SandboxBuildError> {
        let platform = self.platform;
        if platform != SandboxPlatform::Linux {
            if let Some(runtime) = request.runtime {
                return Err(SandboxBuildError::LinuxOnlyRuntime { platform, runtime });
            }
            if request.isolation == Some(KrunIsolation::Full) {
                return Err(SandboxBuildError::FullIsolationUnavailable { platform });
            }
        }
        Ok(SandboxPlan {
            isolation: request
                .isolation
                .unwrap_or_else(|| platform.default_isolation()),
            runtime: request.runtime.or_else(|| platform.default_runtime()),
            network: request.network,
            allow_write: request.allow_write.clone(),
        })
    }

    /// Resolve, create and start the sandbox for `workdir`.
    pub async fn start(
        &self,
        workdir: &Path,
        request: &SandboxRequest,
    ) -> Result<(Arc<dyn Sandbox>, SandboxPlan), SandboxBuildError> {
        let plan = self.resolve(request)?;
        if !self
            .registry
            .kinds()
            .iter()
            .any(|kind| kind == LOCAL_SANDBOX_KIND)
        {
            return Err(SandboxBuildError::Unavailable {
                platform: self.platform,
            });
        }
        let sandbox = self
            .registry
            .create(LOCAL_SANDBOX_KIND, plan.config(workdir))
            .await
            .map_err(|e| SandboxBuildError::Start(e.to_string()))?;
        if let Err(e) = sandbox.start().await {
            let _ = sandbox.destroy().await;
            return Err(SandboxBuildError::Start(e.to_string()));
        }
        Ok((sandbox, plan))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use loop_agent::harness::{
        ExecutionEnv, HostExecutionEnv, SandboxError, SandboxFactory, SandboxInfo, SandboxStatus,
    };
    use parking_lot::Mutex;

    struct FakeSandbox {
        env: Arc<dyn ExecutionEnv>,
        fail_start: bool,
    }

    #[async_trait]
    impl Sandbox for FakeSandbox {
        fn kind(&self) -> &str {
            LOCAL_SANDBOX_KIND
        }
        fn id(&self) -> &str {
            "fake"
        }
        fn status(&self) -> SandboxStatus {
            SandboxStatus::Ready
        }
        fn info(&self) -> SandboxInfo {
            SandboxInfo::enabled("fake", vec![])
        }
        fn env(&self) -> Arc<dyn ExecutionEnv> {
            Arc::clone(&self.env)
        }
        async fn start(&self) -> Result<(), SandboxError> {
            if self.fail_start {
                Err(SandboxError::StartFailed("podman missing".into()))
            } else {
                Ok(())
            }
        }
        async fn stop(&self) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn destroy(&self) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeFactory {
        configs: Mutex<Vec<SandboxConfig>>,
        fail_start: bool,
    }

    #[async_trait]
    impl SandboxFactory for FakeFactory {
        fn kind(&self) -> &str {
            LOCAL_SANDBOX_KIND
        }
        async fn create(&self, config: SandboxConfig) -> Result<Arc<dyn Sandbox>, SandboxError> {
            let env: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(&config.workdir));
            self.configs.lock().push(config);
            Ok(Arc::new(FakeSandbox {
                env,
                fail_start: self.fail_start,
            }))
        }
    }

    fn builder_with(
        platform: SandboxPlatform,
        factory: Option<Arc<FakeFactory>>,
    ) -> SandboxBuilder {
        let registry = SandboxRegistry::new();
        if let Some(factory) = factory {
            registry.register(factory);
        }
        SandboxBuilder::new(platform, registry)
    }

    fn flags(args: &[&str]) -> SandboxRequest {
        SandboxRequest::for_flags(
            &SandboxSettings::default(),
            parse_local_sandbox_flags(args).unwrap(),
        )
    }

    #[test]
    fn flags_left_out_stay_unset() {
        assert_eq!(
            parse_local_sandbox_flags(&[]).unwrap(),
            LocalSandboxFlags::default()
        );
        let parsed = parse_local_sandbox_flags(&["--partial", "--krun"]).unwrap();
        assert_eq!(parsed.isolation, Some(KrunIsolation::Partial));
        assert_eq!(parsed.runtime, Some(LocalSandboxRuntime::Krun));
        let parsed = parse_local_sandbox_flags(&["full", "gvisor"]).unwrap();
        assert_eq!(parsed.isolation, Some(KrunIsolation::Full));
        assert_eq!(parsed.runtime, Some(LocalSandboxRuntime::Runsc));
        assert!(parse_local_sandbox_flags(&["--crun", "--krun"]).is_err());
        assert!(parse_local_sandbox_flags(&["--full", "--partial"]).is_err());
        assert!(parse_local_sandbox_flags(&["--docker"]).is_err());
    }

    #[test]
    fn linux_defaults_are_full_runc() {
        let builder = builder_with(SandboxPlatform::Linux, None);
        let plan = builder.resolve(&SandboxRequest::default()).unwrap();
        assert_eq!(plan.isolation, KrunIsolation::Full);
        assert_eq!(plan.runtime, Some(LocalSandboxRuntime::Runc));
        assert_eq!(plan.label(), "--full --runc");
    }

    #[test]
    fn linux_accepts_every_flag() {
        let builder = builder_with(SandboxPlatform::Linux, None);
        for args in [
            &["--full", "--runc"][..],
            &["--partial", "--crun"],
            &["--runsc"],
            &["--krun"],
        ] {
            assert!(builder.resolve(&flags(args)).is_ok(), "{args:?}");
        }
        let plan = builder.resolve(&flags(&["--partial", "--krun"])).unwrap();
        assert_eq!(plan.label(), "--partial --krun");
    }

    #[test]
    fn macos_defaults_are_partial_without_runtime() {
        let builder = builder_with(SandboxPlatform::MacOs, None);
        let plan = builder.resolve(&SandboxRequest::default()).unwrap();
        assert_eq!(plan.isolation, KrunIsolation::Partial);
        assert_eq!(plan.runtime, None);
        assert_eq!(plan.label(), "--partial");
        assert!(builder.resolve(&flags(&["--partial"])).is_ok());
    }

    #[test]
    fn macos_rejects_explicit_linux_only_flags() {
        let builder = builder_with(SandboxPlatform::MacOs, None);
        for flag in ["--runc", "--crun", "--runsc", "--krun"] {
            let err = builder.resolve(&flags(&[flag])).unwrap_err();
            assert!(
                matches!(err, SandboxBuildError::LinuxOnlyRuntime { .. }),
                "{flag}: {err:?}"
            );
            let message = err.to_string();
            assert!(message.contains(flag), "{message}");
            assert!(message.contains("macOS"), "{message}");
        }
        let err = builder.resolve(&flags(&["--full"])).unwrap_err();
        assert_eq!(
            err,
            SandboxBuildError::FullIsolationUnavailable {
                platform: SandboxPlatform::MacOs
            }
        );
        assert!(err.to_string().contains("--full"));
    }

    #[test]
    fn macos_rejects_explicit_linux_only_settings_but_not_inherited_ones() {
        let builder = builder_with(SandboxPlatform::MacOs, None);
        let inherited = SandboxSettings {
            mode: "local".into(),
            ..Default::default()
        };
        let request = SandboxRequest::from_settings(&inherited).unwrap();
        assert!(builder.resolve(&request).is_ok());

        let explicit = SandboxSettings {
            runtime: Some("runc".into()),
            ..inherited.clone()
        };
        let request = SandboxRequest::from_settings(&explicit).unwrap();
        let err = builder.resolve(&request).unwrap_err();
        assert!(err.to_string().contains("sandbox.runtime"), "{err}");

        let explicit = SandboxSettings {
            isolation: Some("full".into()),
            ..inherited
        };
        let request = SandboxRequest::from_settings(&explicit).unwrap();
        assert!(matches!(
            builder.resolve(&request),
            Err(SandboxBuildError::FullIsolationUnavailable { .. })
        ));
    }

    fn local_settings(isolation: Option<&str>, runtime: Option<&str>) -> SandboxRequest {
        SandboxRequest::from_settings(&SandboxSettings {
            mode: "local".into(),
            isolation: isolation.map(Into::into),
            runtime: runtime.map(Into::into),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn legacy_full_runc_pair_is_read_as_unset() {
        let request = local_settings(Some("full"), Some("runc"));
        assert_eq!(request.isolation, None);
        assert_eq!(request.runtime, None);

        let mac = builder_with(SandboxPlatform::MacOs, None)
            .resolve(&request)
            .unwrap();
        assert_eq!(mac.label(), "--partial");
        let linux = builder_with(SandboxPlatform::Linux, None)
            .resolve(&request)
            .unwrap();
        assert_eq!(linux.label(), "--full --runc");
    }

    #[test]
    fn only_the_exact_legacy_pair_is_forgiven_on_macos() {
        let mac = builder_with(SandboxPlatform::MacOs, None);
        for (isolation, runtime) in [
            (None, Some("runc")),
            (Some("full"), None),
            (Some("full"), Some("krun")),
            (Some("partial"), Some("runc")),
            (Some("full"), Some("crun")),
        ] {
            let request = local_settings(isolation, runtime);
            assert!(
                mac.resolve(&request).is_err(),
                "{isolation:?} {runtime:?} should be explicit"
            );
        }
    }

    #[test]
    fn typed_full_runc_still_errors_on_macos() {
        let mac = builder_with(SandboxPlatform::MacOs, None);
        let settings = SandboxSettings {
            mode: "local".into(),
            isolation: Some("full".into()),
            runtime: Some("runc".into()),
            ..Default::default()
        };
        for args in [&["--full", "--runc"][..], &["--runc"], &["--full"]] {
            let request =
                SandboxRequest::for_flags(&settings, parse_local_sandbox_flags(args).unwrap());
            assert!(mac.resolve(&request).is_err(), "{args:?}");
        }
    }

    #[test]
    fn unknown_setting_values_are_errors() {
        let settings = SandboxSettings {
            runtime: Some("docker".into()),
            ..Default::default()
        };
        let err = SandboxRequest::from_settings(&settings).unwrap_err();
        assert!(err.to_string().contains("sandbox.runtime"), "{err}");
        let settings = SandboxSettings {
            isolation: Some("half".into()),
            ..Default::default()
        };
        assert!(SandboxRequest::from_settings(&settings).is_err());
    }

    #[test]
    fn flags_replace_saved_isolation_and_runtime_but_keep_network() {
        let settings = SandboxSettings {
            isolation: Some("partial".into()),
            runtime: Some("docker".into()),
            network: Some(true),
            allow_write: vec!["~/.cargo".into()],
            ..Default::default()
        };
        let request = SandboxRequest::for_flags(&settings, LocalSandboxFlags::default());
        assert_eq!(request.isolation, None);
        assert_eq!(request.runtime, None);
        assert!(request.network);
        let home = dirs::home_dir().unwrap();
        assert_eq!(request.allow_write, vec![home.join(".cargo")]);

        // A bad saved runtime does not block the flags that replace it.
        let request =
            SandboxRequest::for_flags(&settings, parse_local_sandbox_flags(&["--crun"]).unwrap());
        let plan = builder_with(SandboxPlatform::Linux, None)
            .resolve(&request)
            .unwrap();
        assert_eq!(plan.label(), "--full --crun");
    }

    #[tokio::test]
    async fn linux_builds_from_the_registered_local_factory() {
        let factory = Arc::new(FakeFactory::default());
        let builder = builder_with(SandboxPlatform::Linux, Some(Arc::clone(&factory)));
        let workdir = std::env::temp_dir();
        let absolute = workdir.join("cache");
        let request = SandboxRequest {
            network: true,
            allow_write: vec![absolute.clone(), "target".into()],
            ..flags(&["--partial", "--crun"])
        };
        let (sandbox, plan) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            builder.start(&workdir, &request),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(sandbox.kind(), LOCAL_SANDBOX_KIND);
        assert_eq!(plan.label(), "--partial --crun");
        let configs = factory.configs.lock();
        assert_eq!(configs.len(), 1);
        let options = &configs[0].options;
        assert_eq!(configs[0].workdir, workdir);
        assert_eq!(options["isolation"], "partial");
        assert_eq!(options["runtime"], "crun");
        assert_eq!(options["network"], true);
        assert_eq!(
            options["allowWrite"],
            json!([
                absolute.display().to_string(),
                workdir.join("target").display().to_string()
            ])
        );
    }

    #[tokio::test]
    async fn missing_platform_factory_says_not_available() {
        let builder = builder_with(SandboxPlatform::MacOs, None);
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            builder.start(&std::env::temp_dir(), &SandboxRequest::default()),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert_eq!(
            err,
            SandboxBuildError::Unavailable {
                platform: SandboxPlatform::MacOs
            }
        );
        assert!(err.to_string().contains("not available on macOS yet"));
    }

    #[tokio::test]
    async fn explicit_flag_error_wins_over_missing_factory() {
        let builder = builder_with(SandboxPlatform::MacOs, None);
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            builder.start(&std::env::temp_dir(), &flags(&["--krun"])),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert!(matches!(err, SandboxBuildError::LinuxOnlyRuntime { .. }));
    }

    #[tokio::test]
    async fn start_failure_is_reported() {
        let factory = Arc::new(FakeFactory {
            fail_start: true,
            ..Default::default()
        });
        let builder = builder_with(SandboxPlatform::Linux, Some(factory));
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            builder.start(&std::env::temp_dir(), &SandboxRequest::default()),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert!(err.to_string().contains("podman missing"), "{err}");
    }

    #[test]
    fn platform_registry_matches_the_build_target() {
        let kinds = platform_registry().kinds();
        let has_local = kinds.iter().any(|k| k == LOCAL_SANDBOX_KIND);
        assert_eq!(has_local, cfg!(target_os = "linux"), "{kinds:?}");
    }
}
