//! The TUI's typed client boundary over today's in-process native backend.
//!
//! The host is private and there is no Deref to CliRuntime or Runtime. Deferred
//! groups have an explicit, temporary `legacy` escape hatch (see the design TODO).
use loop_protocol::{rpc::*, AgentBackend, SessionRef, SessionSpec};
use loop_wire::RuntimeControl;
use std::sync::Arc;
mod native_sessions;
use crate::theme::Theme;
use crate::{keybindings, CliRuntime, PendingAgent, StartupSessions, ToolApprovalBridge};
mod legacy;
pub use legacy::LegacyHost;

/// A Surface's client services and local terminal state.
///
/// The client cannot be used as the application host:
/// ```compile_fail
/// fn host(client: &mut loop_cli::SurfaceClient) {
///     let _: &mut loop_app_core::Runtime = client;
/// }
/// ```
/// Nor can a child module reach a hidden host through field access:
/// ```compile_fail
/// fn host(client: &loop_cli::SurfaceClient) {
///     let _ = &client.host.inner;
/// }
/// ```
pub struct SurfaceClient {
    host: LegacyHost,
    sessions: Arc<dyn RuntimeControl>,
    native_sessions: Arc<native_sessions::NativeSessions>,
    native_backend: Arc<dyn AgentBackend>,
    workspace_id: String,
    /// Terminal keybindings.
    pub keybindings: keybindings::Keybindings,
    /// Debug session logging enabled.
    pub debug: bool,
    /// Path to the active debug log file, if any.
    pub debug_log_path: Option<std::path::PathBuf>,
    /// External agent to start once the surface is ready.
    pub pending_agent: Option<PendingAgent>,
    /// Subscription being started (`id` or `id/model`), until it is ready or fails.
    pub starting_agent: Option<String>,
    /// Subscription being signed in to from `/login`, until it finishes.
    pub signing_in: Option<String>,
    /// Backend instances whose updates are already forwarded to the surface.
    pub forwarded: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Recoverable reasoning warnings already shown in this CLI process.
    pub(crate) reasoning_notices: std::collections::HashSet<String>,
    /// `/permissions full-access` was asked for once and needs repeating to
    /// confirm.
    pub confirm_full_access: bool,
    /// `/export` was asked to overwrite files with this path argument and
    /// needs repeating to confirm.
    pub confirm_export: Option<String>,
    /// `--resume` with no id or `--continue`: what to do once the TUI is up.
    pub startup_sessions: Option<StartupSessions>,
    /// `/usage` asked for the usage view; the chat loop opens it.
    pub usage_requested: bool,
}

impl From<CliRuntime> for SurfaceClient {
    fn from(runtime: CliRuntime) -> Self {
        let CliRuntime {
            inner,
            keybindings,
            debug,
            debug_log_path,
            pending_agent,
            starting_agent,
            signing_in,
            forwarded,
            reasoning_notices,
            confirm_full_access,
            confirm_export,
            startup_sessions,
            usage_requested,
        } = runtime;
        let native = Arc::new(native_sessions::NativeSessions::new(
            inner.native.clone(),
            inner.session_store.clone(),
            inner.cwd.clone(),
        ));
        let workspace_id = native.workspace_id.clone();
        Self {
            sessions: native.clone(),
            native_sessions: native.clone(),
            native_backend: native,
            workspace_id,
            host: LegacyHost { inner },
            keybindings,
            debug,
            debug_log_path,
            pending_agent,
            starting_agent,
            signing_in,
            forwarded,
            reasoning_notices,
            confirm_full_access,
            confirm_export,
            startup_sessions,
            usage_requested,
        }
    }
}

impl SurfaceClient {
    /// Temporary access for the unmigrated groups 3–6 only.
    pub fn legacy(&self) -> &LegacyHost {
        &self.host
    }
    /// Temporary mutation access for the unmigrated groups 3–6 only.
    pub fn legacy_mut(&mut self) -> &mut LegacyHost {
        &mut self.host
    }
    pub fn backend(&self) -> &Arc<dyn AgentBackend> {
        if self.is_native() {
            &self.native_backend
        } else {
            &self.host.inner.backend
        }
    }
    pub fn agent_dir(&self) -> &std::path::PathBuf {
        &self.host.inner.agent_dir
    }
    pub fn cwd(&self) -> &std::path::PathBuf {
        &self.host.inner.cwd
    }
    pub fn theme(&self) -> &Theme {
        &self.host.inner.theme
    }
    pub fn session_id(&self) -> &String {
        &self.host.inner.session_id
    }
    pub fn selected_model(&self) -> &Option<loop_ai::Model> {
        &self.host.inner.selected_model
    }
    pub fn model_note(&self) -> &Option<String> {
        &self.host.inner.model_note
    }
    pub fn needs_provider_setup(&self) -> bool {
        self.host.inner.needs_provider_setup
    }
    pub fn resumed(&self) -> bool {
        self.host.inner.resumed
    }
    pub fn sessions_db(&self) -> &std::path::PathBuf {
        &self.host.inner.sessions_db
    }
    pub fn active_agent(&self) -> &Option<String> {
        &self.host.inner.active_agent
    }
    pub fn agent_model(&self) -> &Option<String> {
        &self.host.inner.agent_model
    }
    pub fn agent_effort(&self) -> &Option<String> {
        &self.host.inner.agent_effort
    }
    pub fn take_startup_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.host.inner.startup_notices)
    }
    pub fn set_theme(&mut self, theme: Theme) {
        self.host.inner.theme = theme;
    }
    pub fn set_resumed(&mut self, resumed: bool) {
        self.host.inner.resumed = resumed;
    }
    pub fn route_agent_replay(&mut self, id: &str) {
        self.host.inner.session_id = id.into();
    }
    /// Ask the native harness to stop work as the TUI exits.
    pub fn request_shutdown(&self) {
        self.host.inner.harness.request_shutdown();
    }
    pub fn shutdown(&self) -> loop_app_core::runtime::SessionShutdown {
        self.host.inner.session_shutdown()
    }
    pub fn is_native(&self) -> bool {
        self.host.inner.is_native()
    }
    pub fn is_subscription(&self, provider: &str) -> bool {
        self.host.inner.is_subscription(provider)
    }
    pub fn active_model_label(&self) -> Option<String> {
        self.host.inner.active_model_label()
    }
    pub fn native_effort(&self) -> String {
        self.host.inner.native_effort()
    }
    pub fn selected_model_spec(&self) -> Option<String> {
        self.host.inner.selected_model_spec()
    }
    pub fn session_ref(&self) -> loop_protocol::SessionRef {
        self.host.inner.session_ref()
    }
    pub fn note_prompt(&self, prompt: &str) {
        self.host.inner.note_prompt(prompt)
    }
    pub fn adopt_agent(
        &mut self,
        id: &str,
        backend: Arc<dyn loop_protocol::AgentBackend>,
        session: loop_protocol::SessionRef,
        model: Option<String>,
    ) {
        self.host.inner.adopt_agent(id, backend, session, model)
    }
    pub async fn sync_agent_effort(&mut self) {
        self.host.inner.sync_agent_effort().await
    }
    pub async fn use_native(&mut self) {
        self.host.inner.use_native().await
    }
    /// The model catalog, for tests that compare the picker with it.
    #[cfg(test)]
    pub(crate) fn catalog(&self) -> &Arc<loop_ai::Models> {
        &self.host.inner.models
    }
    /// S3.3's typed Session control service. No filesystem paths cross it.
    pub fn sessions(&self) -> &dyn RuntimeControl {
        self.sessions.as_ref()
    }

    pub async fn list_sessions(
        &self,
    ) -> anyhow::Result<Vec<loop_app_core::sessions::SessionChoice>> {
        let mut choices: Vec<_> = self
            .sessions
            .list()
            .await?
            .into_iter()
            // Preserve the TUI's message-only resume picker, while the service
            // also supplies empty related Sessions to /tree.
            .filter(|info| info.summary.message_count.unwrap_or(0) > 0)
            .map(|info| loop_app_core::sessions::SessionChoice {
                id: info.r#ref.native_id,
                title: info
                    .name
                    .or(info.summary.first_prompt)
                    .unwrap_or_else(|| "(no title)".into()),
                source: info.summary.selected_model,
                message_count: info.summary.message_count.map(|n| n as usize),
                updated_at: info.last_active_ms as i64,
                agent: None,
            })
            .collect();
        choices.extend(loop_app_core::sessions::merge(
            vec![],
            self.host.inner.agents.sessions_in(self.cwd()),
        ));
        choices.sort_by_key(|choice| std::cmp::Reverse(choice.updated_at));
        Ok(choices)
    }

    pub async fn open_native_session(&mut self, id: &str) -> anyhow::Result<()> {
        let session = SessionRef {
            instance: self.native_backend.instance().clone(),
            key: id.into(),
            native_id: id.into(),
        };
        self.native_backend
            .load_session(&session, SessionSpec::default())
            .await?;
        self.host.inner.use_native().await;
        self.host.inner.resumed = true;
        self.host.inner.active_skills.clear();
        self.refresh_native_model();
        self.host.restore_session_approvals().await;
        Ok(())
    }

    fn refresh_native_model(&mut self) {
        let config = self.host.inner.native.config();
        self.host.inner.selected_model = config.config.model.as_deref().and_then(|spec| {
            let (provider, id) = spec.split_once('/')?;
            self.host.inner.models.get_model(provider, id)
        });
        self.host.inner.model_note = None;
    }

    pub async fn new_session(&mut self) -> anyhow::Result<()> {
        if self.is_native() {
            let session = self
                .native_backend
                .new_session(SessionSpec {
                    cwd: Some(self.cwd().clone()),
                    name: None,
                })
                .await?;
            self.host.inner.session_id = session.native_id;
            self.refresh_native_model();
        } else {
            self.host.inner.new_session().await?;
        }
        self.host.inner.resumed = false;
        self.host.inner.active_skills.clear();
        self.host.clear_session_approvals();
        Ok(())
    }

    pub async fn rename_session(&self, name: &str) -> anyhow::Result<()> {
        if !self.is_native() {
            self.host.inner.agents.rename(self.session_id(), name)?;
            return Ok(());
        }
        self.sessions
            .rename(RenameParams {
                session: self.session_ref(),
                name: name.into(),
                expected_revision: None,
            })
            .await?;
        Ok(())
    }

    pub async fn session_tree(&self) -> anyhow::Result<TreeResult> {
        let mut tree = self
            .sessions
            .tree(TreeParams {
                session: self.session_ref(),
                cursor: None,
                limit: Some(1000),
            })
            .await?;
        while let Some(cursor) = tree.next_cursor.take() {
            let page = self
                .sessions
                .tree(TreeParams {
                    session: self.session_ref(),
                    cursor: Some(cursor),
                    limit: Some(1000),
                })
                .await?;
            if page.branch_revision != tree.branch_revision {
                anyhow::bail!("tree changed; open /tree again");
            }
            tree.entries.extend(page.entries);
            tree.fork_points.extend(page.fork_points);
            tree.next_cursor = page.next_cursor;
        }
        Ok(tree)
    }

    pub async fn session_family(&self, tree: &TreeResult) -> anyhow::Result<Vec<(String, String)>> {
        let infos = self.sessions.list().await?;
        Ok(tree
            .family
            .iter()
            .map(|session| {
                let info = infos.iter().find(|info| info.r#ref == *session);
                let title = info
                    .and_then(|info| info.name.as_ref().or(info.summary.first_prompt.as_ref()))
                    .unwrap_or(&session.native_id);
                let child = info
                    .and_then(|info| info.parent.as_ref())
                    .is_some_and(|parent| parent.native_id == *self.session_id());
                (
                    session.native_id.clone(),
                    format!("{} · {title}", if child { "fork" } else { "forked from" }),
                )
            })
            .collect())
    }

    pub async fn navigate(&mut self, target_entry_id: Option<String>) -> anyhow::Result<()> {
        self.sessions
            .navigate(NavigateParams {
                session: self.session_ref(),
                target_entry_id,
                summarize: false,
                expected_revision: None,
            })
            .await?;
        self.refresh_native_model();
        Ok(())
    }

    /// Create first, then load the returned destination. Source copies never use
    /// the old mutating harness fork helper.
    pub async fn fork_session(
        &mut self,
        before_entry_id: Option<&str>,
        name: Option<String>,
    ) -> anyhow::Result<DestinationResult> {
        let session = self.session_ref();
        if self.backend().is_busy(&session) {
            anyhow::bail!("wait for the current turn to finish, then fork or clone");
        }
        let request_id = loop_ai::new_id();
        let destination = match before_entry_id {
            Some(before) => {
                self.sessions
                    .fork(ForkParams {
                        session,
                        before_entry_id: before.into(),
                        name,
                        request_id,
                    })
                    .await?
            }
            None => {
                self.sessions
                    .clone_session(CloneParams {
                        session,
                        name,
                        request_id,
                    })
                    .await?
            }
        };
        if let Err(error) = self
            .open_native_session(&destination.session.native_id)
            .await
        {
            self.native_sessions
                .discard_destination(&destination.session)
                .await
                .map_err(|cleanup| {
                    anyhow::anyhow!(
                        "{error:#}; the unused copy {} could not be removed ({cleanup}); \
                         delete it from /sessions",
                        destination.session.native_id
                    )
                })?;
            return Err(error);
        }
        Ok(destination)
    }

    pub async fn export_payload(&self) -> anyhow::Result<ExportResult> {
        if !self.is_native() {
            anyhow::bail!(
                "/export saves Loop's own sessions; this one runs in a subscription agent"
            );
        }
        Ok(self
            .sessions
            .export(SessionParams {
                session: self.session_ref(),
            })
            .await?)
    }

    pub async fn import_payload(&self, jsonl: String) -> anyhow::Result<DestinationResult> {
        Ok(self
            .sessions
            .import(ImportParams {
                workspace_id: self.workspace_id.clone(),
                jsonl,
                request_id: loop_ai::new_id(),
            })
            .await?)
    }

    fn native_control(&self) -> anyhow::Result<&dyn loop_protocol::ModelControl> {
        self.native_backend
            .model_control()
            .ok_or_else(|| anyhow::anyhow!("Loop's own agent offers no model choice here"))
    }

    /// The native Session's model choices, in picker order.
    pub async fn available_models(&self) -> anyhow::Result<Vec<loop_ai::Model>> {
        let session = self.host.inner.native.current_session().await;
        let choices = self.native_control()?.models(&session).await?;
        let models = choices
            .models
            .into_iter()
            .filter_map(|choice| {
                let (provider, id) = choice.id.split_once('/')?;
                self.host.inner.models.get_model(provider, id)
            })
            .collect();
        Ok(crate::config::providers::sort_models_for_picker(models))
    }

    pub async fn cycle_models(&self) -> anyhow::Result<Vec<loop_ai::Model>> {
        let models = self.available_models().await?;
        let enabled = &self.host.inner.settings.enabled_models;
        if enabled.is_empty() {
            return Ok(models);
        }
        Ok(models
            .into_iter()
            .filter(|model| {
                enabled
                    .iter()
                    .any(|id| id == &model.id || id == &format!("{}/{}", model.provider, model.id))
            })
            .collect())
    }

    pub async fn resolve_model(&self, spec: &str) -> anyhow::Result<loop_ai::Model> {
        // Preserve bare ids, provider/model ids and the existing ambiguity help.
        loop_app_core::model_selection::resolve_model_spec(&self.host.inner.models, spec)
    }

    pub async fn select_model(
        &mut self,
        provider: &str,
        id: &str,
    ) -> anyhow::Result<loop_ai::Model> {
        if loop_ai::CredentialStore::is_logged_out(self.host.inner.credentials.as_ref(), provider) {
            anyhow::bail!("{provider} is logged out; run /login {provider}");
        }
        let model = self.resolve_model(&format!("{provider}/{id}")).await?;
        let session = self.host.inner.native.current_session().await;
        self.native_control()?
            .set_model(&session, &format!("{provider}/{id}"), None)
            .await?;
        self.host.inner.selected_model = Some(model.clone());
        self.host.inner.model_note = None;
        Ok(model)
    }

    pub async fn efforts(&self) -> anyhow::Result<(Vec<String>, String)> {
        let choices = self
            .backend()
            .model_control()
            .ok_or_else(|| anyhow::anyhow!("this subscription has no reasoning effort setting"))?
            .efforts(&self.session_ref())
            .await?;
        Ok((
            choices.models.into_iter().map(|choice| choice.id).collect(),
            choices.current.unwrap_or_default(),
        ))
    }

    pub async fn set_effort(&mut self, level: &str) -> anyhow::Result<String> {
        let level = level.trim().to_lowercase();
        // Checked here for every backend, so an agent's refusal reads the same.
        let (levels, _) = self.efforts().await?;
        if !levels.contains(&level) {
            anyhow::bail!(
                "unknown effort `{level}`; choose one of: {}",
                levels.join(", ")
            );
        }
        self.backend()
            .model_control()
            .ok_or_else(|| anyhow::anyhow!("this subscription has no reasoning effort setting"))?
            .set_effort(&self.session_ref(), &level, None)
            .await?;
        if !self.is_native() {
            self.host.inner.agent_effort = Some(level.clone());
        }
        Ok(level)
    }

    /// The configured effort of the current Session (not limited by the
    /// model), as `/default` saves it.
    async fn session_effort(&self) -> anyhow::Result<String> {
        Ok(self.efforts().await?.1)
    }

    /// Whether the current Session already runs on the saved defaults for
    /// new Sessions, so there is nothing for `/default` to save.
    pub async fn on_defaults(&self) -> bool {
        let settings = &self.host.inner.settings;
        let effort = self.session_effort().await.unwrap_or_default();
        match self.active_agent() {
            None => {
                let model = self
                    .selected_model()
                    .as_ref()
                    .map(|m| (m.provider.clone(), m.id.clone()));
                let saved = settings
                    .selected_model()
                    .map(|(p, m)| (p.to_owned(), m.to_owned()));
                model == saved && effort == settings.default_thinking_level
            }
            Some(id) => {
                let saved = settings.selected_model();
                saved.is_some_and(|(p, m)| p == id && Some(m) == self.agent_model().as_deref())
                    && self.host.inner.agents.effort(id).unwrap_or_default() == effort
            }
        }
    }

    /// `/default`: save the current Session's model and effort as the
    /// defaults for new Sessions. Returns what was saved, as `model · effort`.
    ///
    /// Today this writes the host settings in process. Over a Runtime it is
    /// `runtime/defaults` (see [`save_defaults_on`]), which is host-only.
    pub async fn save_defaults(&mut self) -> anyhow::Result<String> {
        let effort = self.session_effort().await?;
        let label = match self.active_agent().clone() {
            None => {
                let model = self
                    .selected_model()
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("choose a model first: /model"))?;
                let inner = &mut self.host.inner;
                inner.settings.default_thinking_level = effort.clone();
                inner
                    .native
                    .set_default_effort(loop_agent::effort::parse_thinking(&effort));
                // Saves the settings file too.
                inner.remember_model(&model.provider, &model.id)?;
                format!("{}/{}", model.provider, model.id)
            }
            Some(id) => {
                let model = self.agent_model().clone();
                let inner = &mut self.host.inner;
                if let Some(model) = &model {
                    inner.remember_model(&id, model)?;
                }
                if !effort.is_empty() {
                    inner.agents.remember_effort(&id, &effort)?;
                }
                model.map_or(id.clone(), |m| format!("{id}/{m}"))
            }
        };
        Ok(if effort.is_empty() {
            label
        } else {
            format!("{label} · {effort}")
        })
    }

    pub async fn cycle_effort(&mut self) -> anyhow::Result<String> {
        let (mut levels, current) = self.efforts().await?;
        if self.is_native() {
            levels.retain(|level| loop_agent::effort::CYCLE_EFFORTS.contains(&level.as_str()));
        }
        let next = levels
            .iter()
            .position(|level| level == &current)
            .map_or(0, |i| (i + 1) % levels.len());
        let level = levels
            .get(next)
            .ok_or_else(|| anyhow::anyhow!("no reasoning effort levels on offer"))?;
        self.set_effort(level).await
    }
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use loop_agent::harness::session::{sqlite::SqliteSessionStore, SessionFile, SessionStore};
    use loop_protocol::{ContentBlock, TurnOutcome};
    use std::time::Duration;
    use test_support::{fixture, pause_provider};

    #[tokio::test]
    async fn copy_and_export_capture_committed_output_without_switching_or_cancelling_source() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let client = fixture(dir.path()).await;
            let session = client.session_ref();
            client
                .backend()
                .prompt(&session, vec![ContentBlock::text("committed")])
                .await
                .unwrap()
                .outcome()
                .await;
            let started = Arc::new(tokio::sync::Notify::new());
            let finish = Arc::new(tokio::sync::Notify::new());
            pause_provider(&client, started.clone(), finish.clone());
            let turn = client
                .backend()
                .prompt(&session, vec![ContentBlock::text("unfinished")])
                .await
                .unwrap();
            started.notified().await;
            let exported = client.export_payload().await.unwrap();
            assert!(exported.markdown.contains("committed"));
            assert!(!exported.markdown.contains("unfinished"));
            let destination = client
                .sessions()
                .clone_session(CloneParams {
                    session: session.clone(),
                    name: None,
                    request_id: "copy-once".into(),
                })
                .await
                .unwrap();
            assert_eq!(client.session_ref(), session);
            assert!(client.backend().is_busy(&session));
            let store = SqliteSessionStore::open(dir.path().join("sessions.db")).unwrap();
            assert!(store
                .lease(&destination.session.native_id)
                .await
                .unwrap()
                .is_some());
            let reader = store.load(&destination.session.native_id).await.unwrap();
            let file = SessionFile::from_reader(reader.as_ref()).await.unwrap();
            assert!(file.markdown().contains("committed"));
            assert!(!file.markdown().contains("unfinished"));
            finish.notify_one();
            assert!(matches!(
                turn.outcome().await,
                TurnOutcome::Completed { .. }
            ));
            client.shutdown().run().await.unwrap();
            assert!(store
                .lease(&destination.session.native_id)
                .await
                .unwrap()
                .is_none());
            assert!(store.lease(&session.native_id).await.unwrap().is_none());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn import_rejects_bad_links_and_oversize_payloads_before_creating_a_destination() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let client = fixture(dir.path()).await;
            let source = client.session_ref();
            client
                .backend()
                .prompt(&source, vec![ContentBlock::text("hello")])
                .await
                .unwrap()
                .outcome()
                .await;
            let before = client.list_sessions().await.unwrap();
            let payload = client.export_payload().await.unwrap();
            let mut lines: Vec<_> = payload.jsonl.lines().map(str::to_owned).collect();
            let mut entry: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
            entry["parent_id"] = "missing".into();
            lines[1] = entry.to_string();
            assert!(client
                .import_payload(lines.join("\n"))
                .await
                .unwrap_err()
                .to_string()
                .contains("parent links"));
            assert!(client
                .import_payload("x".repeat(SESSION_TRANSFER_LIMIT + 1))
                .await
                .unwrap_err()
                .to_string()
                .contains("8 MiB"));
            assert_eq!(client.list_sessions().await.unwrap(), before);
            assert_eq!(client.session_ref(), source);
            client.shutdown().run().await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod paging_tests {
    use super::*;
    #[tokio::test]
    async fn a_tree_cursor_fails_when_new_turn_output_changes_the_rows() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let client = test_support::fixture(dir.path()).await;
            let session = client.session_ref();
            client
                .backend()
                .prompt(&session, vec![loop_protocol::ContentBlock::text("first")])
                .await
                .unwrap()
                .outcome()
                .await;
            let first = client
                .sessions()
                .tree(TreeParams {
                    session: session.clone(),
                    cursor: None,
                    limit: Some(1),
                })
                .await
                .unwrap();
            client
                .backend()
                .prompt(&session, vec![loop_protocol::ContentBlock::text("second")])
                .await
                .unwrap()
                .outcome()
                .await;
            let result = client
                .sessions()
                .tree(TreeParams {
                    session,
                    cursor: first.next_cursor,
                    limit: Some(1),
                })
                .await;
            assert!(result.unwrap_err().to_string().contains("tree changed"));
            client.shutdown().run().await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod destination_retry_tests {
    use super::*;
    #[tokio::test]
    async fn retrying_a_fork_after_source_navigation_returns_the_original_destination() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let mut client = test_support::fixture(dir.path()).await;
            let session = client.session_ref();
            client
                .backend()
                .prompt(&session, vec![loop_protocol::ContentBlock::text("draft")])
                .await
                .unwrap()
                .outcome()
                .await;
            let tree = client.session_tree().await.unwrap();
            let params = ForkParams {
                session,
                before_entry_id: tree.fork_points[0].entry_id.clone(),
                name: None,
                request_id: "same-fork".into(),
            };
            let destination = client.sessions().fork(params.clone()).await.unwrap();
            client.navigate(None).await.unwrap();
            assert_eq!(client.sessions().fork(params).await.unwrap(), destination);
            client.shutdown().run().await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod transfer_limit_tests {
    use super::*;
    #[tokio::test]
    async fn export_applies_the_response_limit_to_json_escaping_as_well_as_jsonl() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let client = test_support::fixture(dir.path()).await;
            let text = "\u{0001}".repeat(1_300_000);
            client
                .backend()
                .prompt(
                    &client.session_ref(),
                    vec![loop_protocol::ContentBlock::text(text)],
                )
                .await
                .unwrap()
                .outcome()
                .await;
            let result = client.export_payload().await;
            assert!(result.is_err(), "an encoded export above 16 MiB must fail");
            let error = result.unwrap_err().to_string();
            assert!(error.contains("16 MiB"), "{error}");
            client.shutdown().run().await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use loop_protocol::{ContentBlock, TurnOutcome};
    use std::time::Duration;

    #[tokio::test]
    async fn closing_a_session_keeps_it_listed_and_loadable() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let mut client = test_support::fixture(dir.path()).await;
            let first = client.session_ref();
            client
                .backend()
                .prompt(&first, vec![ContentBlock::text("keep me")])
                .await
                .unwrap()
                .outcome()
                .await;
            client.new_session().await.unwrap();
            client.sessions().close(&first, false).await.unwrap();
            let listed = client.list_sessions().await.unwrap();
            assert!(listed.iter().any(|c| c.id == first.native_id), "{listed:?}");
            client.open_native_session(&first.native_id).await.unwrap();
            assert_eq!(client.session_ref(), first);
            client.shutdown().run().await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn prompts_work_without_a_session_store() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let client = test_support::fixture_with(dir.path(), false).await;
            let turn = client
                .backend()
                .prompt(&client.session_ref(), vec![ContentBlock::text("hello")])
                .await
                .unwrap();
            assert!(matches!(
                turn.outcome().await,
                TurnOutcome::Completed { .. }
            ));
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn the_workspace_id_is_the_hosts_stable_one() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let one = test_support::fixture(dir.path()).await;
            let expected = loop_runtime::workspace_id(&dir.path().canonicalize().unwrap());
            assert_eq!(one.workspace_id, expected);
            one.shutdown().run().await.unwrap();
            let two = test_support::fixture(dir.path()).await;
            assert_eq!(two.workspace_id, expected);
            two.shutdown().run().await.unwrap();
        })
        .await
        .unwrap();
    }
}
