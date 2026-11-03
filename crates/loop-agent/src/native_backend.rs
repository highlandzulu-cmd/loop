//! [`NativeBackend`]: Loop's own agent loop behind the
//! [`loop_protocol::AgentBackend`] interface.
//!
//! It wraps one [`AgentHarness`], which holds one active session at a time.
//! Agent events are translated by [`NativeEventMapper`] and published as
//! [`UpdateEnvelope`]s on a broadcast stream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use loop_ai::ImageContent;
use loop_protocol::{
    AgentBackend, BackendError, BackendInstanceId, BackendResult, Capabilities, Compaction,
    CompactionReport, ConfigChanged, ContentBlock, ErrorCategory, ErrorInfo, LocalShell,
    ModelChoice, ModelChoices, ModelControl, MutationResult, NoticeLevel, Origin, SessionConfig,
    SessionKey, SessionMetadata, SessionRef, SessionSpec, SessionStats, SessionUpdate, ShellResult,
    StatsSummary, StopReason, TurnHandle, TurnId, TurnOutcome, UpdateEnvelope,
};
use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::effort::{effective_effort, native_efforts, parse_thinking, NATIVE_EFFORTS};
use crate::harness::agent_harness::AgentHarness;
use crate::harness::session::format_session_stats;
use crate::harness::session::types::PendingSessionWrite;
use crate::harness::types::{AgentHarnessError, AgentHarnessPhase, ShellExecOptions};
use crate::protocol_events::{replay, turn_outcome, NativeEventMapper};
use crate::types::AgentThinkingLevel;
use crate::types::{AgentEvent, PromptInput};

/// Instance id of Loop's own agent.
pub const NATIVE_INSTANCE: &str = "loop";

/// Updates buffered per subscriber before a slow reader starts losing the oldest.
const UPDATE_BUFFER: usize = 4096;

/// Loop's own agent loop as an [`AgentBackend`].
pub struct NativeBackend {
    instance: BackendInstanceId,
    harness: Arc<AgentHarness>,
    updates: broadcast::Sender<UpdateEnvelope>,
    stream: Arc<Mutex<StreamState>>,
    /// Set while a prompt sent through this backend runs, so two callers
    /// cannot both start one.
    prompting: Arc<AtomicBool>,
    /// The current Session's model and effort, as last announced.
    config: Arc<Mutex<ConfigState>>,
    /// Serializes model and effort changes, so each revision check and its
    /// write happen together.
    setting: tokio::sync::Mutex<()>,
    /// Effort for a Session that never chose one.
    default_effort: Mutex<AgentThinkingLevel>,
}

/// A Session's config as this backend last announced it.
#[derive(Debug, Clone)]
struct ConfigState {
    /// The Session it describes; another one means it must be read again.
    session: SessionKey,
    revision: u64,
    name: Option<String>,
    /// The effort level chosen for the next turn, before the model's limits.
    level: AgentThinkingLevel,
    /// What the next turn uses.
    config: SessionConfig,
    /// What the running turn uses.
    effective: SessionConfig,
    /// The running turn a change waits for.
    pending: Option<TurnId>,
}

impl ConfigState {
    fn update(&self) -> SessionUpdate {
        SessionUpdate::ConfigChanged(self.changed())
    }

    fn changed(&self) -> ConfigChanged {
        ConfigChanged {
            revision: self.revision,
            metadata: SessionMetadata {
                name: self.name.clone(),
            },
            config: self.config.clone(),
            effective: self.effective.clone(),
            pending: self.pending.is_some(),
            applies_after_turn: self.pending.clone(),
            workspace_revision: 0,
        }
    }
}

/// `provider/id` of `model`.
fn model_id(model: &loop_ai::Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

/// The config a Session runs with on `model` at `level`.
fn session_config(model: Option<&loop_ai::Model>, level: AgentThinkingLevel) -> SessionConfig {
    SessionConfig {
        model: model.map(model_id),
        effort: Some(effective_effort(model, level)),
        mode: None,
    }
}

fn invalid(message: String) -> BackendError {
    BackendError::failed(ErrorCategory::Other, message)
}

/// Clears [`NativeBackend::prompting`] when the prompt is over.
struct PromptSlot(Arc<AtomicBool>);

impl Drop for PromptSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Translation state, shared with the harness subscription.
struct StreamState {
    session: SessionKey,
    seq: u64,
    mapper: NativeEventMapper,
    /// Turns already numbered per Session this backend streamed before, so
    /// switching back never repeats a turn id.
    turns: std::collections::HashMap<SessionKey, u64>,
}

impl NativeBackend {
    /// Wrap `harness`. Subscribes to its events immediately, so call this
    /// before any turn runs. Session changes must go through this backend
    /// (`new_session`, `load_session`, [`NativeBackend::adopt_session`]) so
    /// updates carry the right session key.
    pub async fn new(harness: Arc<AgentHarness>) -> Arc<Self> {
        let (updates, _) = broadcast::channel(UPDATE_BUFFER);
        let session = SessionKey(harness.session_id().await);
        let stream = Arc::new(Mutex::new(StreamState {
            session,
            seq: 0,
            mapper: NativeEventMapper::new(),
            turns: Default::default(),
        }));
        let level = harness.thinking_level().await;
        let backend = Arc::new(Self {
            instance: BackendInstanceId::from(NATIVE_INSTANCE),
            harness: Arc::clone(&harness),
            updates: updates.clone(),
            stream: Arc::clone(&stream),
            prompting: Arc::new(AtomicBool::new(false)),
            config: Arc::new(Mutex::new(ConfigState {
                session: SessionKey(String::new()),
                revision: 0,
                name: None,
                level,
                config: SessionConfig::default(),
                effective: SessionConfig::default(),
                pending: None,
            })),
            setting: tokio::sync::Mutex::new(()),
            default_effort: Mutex::new(level),
        });
        backend.read_config().await;
        let instance = backend.instance.clone();
        let config = Arc::clone(&backend.config);
        harness.subscribe(move |event: AgentEvent| {
            let envelopes = translate(&instance, &stream, &event);
            let ended = envelopes
                .iter()
                .any(|e| matches!(e.update, SessionUpdate::TurnEnded { .. }));
            for envelope in envelopes {
                // No receivers is fine: nobody is listening yet.
                let _ = updates.send(envelope);
            }
            if ended {
                apply_pending(&instance, &stream, &config, &updates);
            }
            async {}
        });
        backend
    }

    /// The current Session's config, as a `config_changed` update would
    /// carry it.
    pub fn config(&self) -> ConfigChanged {
        self.config.lock().changed()
    }

    /// Effort for Sessions that never chose one, used when one is restored.
    pub fn set_default_effort(&self, level: AgentThinkingLevel) {
        *self.default_effort.lock() = level;
    }

    /// The effort level the current Session chose for its next turn, before
    /// the model's limits apply (see [`crate::effort::effective_effort`]).
    pub fn thinking_level(&self) -> AgentThinkingLevel {
        self.config.lock().level
    }

    /// Apply the model and effort the current Session recorded: its model
    /// when still available, and its effort, or the default effort when it
    /// never chose one (an explicit `off` stays off).
    pub async fn restore_config(&self) -> BackendResult<()> {
        let _setting = self.setting.lock().await;
        let recorded = self.harness.session_config().await.map_err(harness_error)?;
        if let Some((provider, id)) = &recorded.model {
            if let Some(model) = self.resolve(provider, id) {
                self.harness.set_model(model).await;
            }
        }
        let level = recorded
            .thinking_level
            .unwrap_or_else(|| *self.default_effort.lock());
        self.harness.set_thinking_level(level).await;
        self.read_config().await;
        Ok(())
    }

    /// `provider/id` as a model the Session may use: known, and its
    /// provider not logged out.
    fn resolve(&self, provider: &str, id: &str) -> Option<loop_ai::Model> {
        let models = self.harness.models();
        if models.credentials().is_logged_out(provider) {
            return None;
        }
        models.get_model(provider, id)
    }

    /// Read the current Session's config from the harness and its tree.
    async fn read_config(&self) {
        let session = SessionKey(self.harness.session_id().await);
        let recorded = self.harness.session_config().await.unwrap_or_default();
        let model = self.harness.model().await;
        let level = self.harness.thinking_level().await;
        let config = session_config(model.as_ref(), level);
        *self.config.lock() = ConfigState {
            session,
            revision: recorded.revision,
            name: recorded.name,
            level,
            effective: config.clone(),
            config,
            pending: None,
        };
    }

    /// Make sure the announced config is the harness's current Session's.
    async fn current_config(&self, session: &SessionRef) -> BackendResult<()> {
        self.check_session(session).await?;
        let stale = self.config.lock().session != session.key;
        if stale {
            self.read_config().await;
        }
        Ok(())
    }

    /// Record a change made under [`Self::setting`], announce it, and say
    /// when it applies.
    async fn commit(&self, write: PendingSessionWrite) -> BackendResult<MutationResult> {
        self.harness
            .record_config(write)
            .await
            .map_err(harness_error)?;
        let model = self.harness.model().await;
        let level = self.harness.thinking_level().await;
        let name = self
            .harness
            .session_config()
            .await
            .ok()
            .and_then(|c| c.name);
        let running = self.is_busy_now();
        let mut state = self.config.lock();
        state.revision += 1;
        if name.is_some() {
            state.name = name;
        }
        state.level = level;
        state.config = session_config(model.as_ref(), level);
        let current_turn = self.stream.lock().mapper.current_turn().cloned();
        match current_turn.filter(|_| running) {
            Some(turn) => state.pending = Some(turn),
            None => {
                state.effective = state.config.clone();
                state.pending = None;
            }
        }
        let envelope = announce(&self.instance, &self.stream, state.update());
        let result = MutationResult {
            revision: state.revision,
            seq: envelope.seq,
            pending: state.pending.is_some(),
            applies_after_turn: state.pending.clone(),
        };
        let _ = self.updates.send(envelope);
        Ok(result)
    }

    fn check_revision(&self, expected: Option<u64>) -> BackendResult<()> {
        let revision = self.config.lock().revision;
        match expected {
            Some(expected) if expected != revision => Err(BackendError::Conflict { revision }),
            _ => Ok(()),
        }
    }

    fn is_busy_now(&self) -> bool {
        self.prompting.load(Ordering::Acquire) || self.harness.phase() != AgentHarnessPhase::Idle
    }

    /// The wrapped harness, for native-only features (sandbox, skills, MCP,
    /// forking, workflows).
    pub fn harness(&self) -> &Arc<AgentHarness> {
        &self.harness
    }

    /// A reference to the harness's current session.
    pub async fn current_session(&self) -> SessionRef {
        self.session_ref(self.harness.session_id().await)
    }

    /// Number this Session's next turns after `turns` earlier ones, recorded
    /// elsewhere (for example in the Runtime's Event Log), so turn ids stay
    /// unique across process restarts.
    pub fn continue_turns_after(&self, turns: u64) {
        self.stream.lock().mapper.continue_after(turns);
    }

    /// Point the update stream at a session the harness switched to by
    /// itself (for example after forking).
    pub fn adopt_session(&self, native_id: &str) -> SessionRef {
        let mut stream = self.stream.lock();
        let (old, started) = (stream.session.clone(), stream.mapper.turns_started());
        let seen = stream.turns.entry(old).or_default();
        *seen = (*seen).max(started);
        stream.session = SessionKey::from(native_id);
        stream.seq = 0;
        stream.mapper = NativeEventMapper::new();
        let earlier = stream.turns.get(&stream.session).copied().unwrap_or(0);
        stream.mapper.continue_after(earlier);
        drop(stream);
        self.session_ref(native_id.to_owned())
    }

    fn session_ref(&self, native_id: String) -> SessionRef {
        SessionRef {
            instance: self.instance.clone(),
            key: SessionKey(native_id.clone()),
            native_id,
        }
    }

    async fn check_session(&self, session: &SessionRef) -> BackendResult<()> {
        if session.instance != self.instance || session.native_id != self.harness.session_id().await
        {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        Ok(())
    }
}

fn translate(
    instance: &BackendInstanceId,
    stream: &Mutex<StreamState>,
    event: &AgentEvent,
) -> Vec<UpdateEnvelope> {
    let mut state = stream.lock();
    let updates = state.mapper.map(event);
    let mut envelopes = Vec::with_capacity(updates.len());
    for update in updates {
        let turn = match &update {
            SessionUpdate::TurnStarted { turn_id } | SessionUpdate::TurnEnded { turn_id, .. } => {
                Some(turn_id.clone())
            }
            _ => state.mapper.current_turn().cloned(),
        };
        state.seq += 1;
        envelopes.push(UpdateEnvelope::live(
            instance.clone(),
            0,
            state.session.clone(),
            turn,
            state.seq,
            update,
        ));
    }
    envelopes
}

/// `update` as the current Session's next envelope, outside any turn.
fn announce(
    instance: &BackendInstanceId,
    stream: &Mutex<StreamState>,
    update: SessionUpdate,
) -> UpdateEnvelope {
    let mut state = stream.lock();
    state.seq += 1;
    UpdateEnvelope::live(
        instance.clone(),
        0,
        state.session.clone(),
        None,
        state.seq,
        update,
    )
}

/// A turn ended: changes made during it now apply. Announce that.
fn apply_pending(
    instance: &BackendInstanceId,
    stream: &Mutex<StreamState>,
    config: &Mutex<ConfigState>,
    updates: &broadcast::Sender<UpdateEnvelope>,
) {
    let mut state = config.lock();
    if state.pending.take().is_none() || state.session != stream.lock().session {
        return;
    }
    state.effective = state.config.clone();
    let _ = updates.send(announce(instance, stream, state.update()));
}

/// An error notice outside any turn.
fn error_notice(
    instance: &BackendInstanceId,
    stream: &Mutex<StreamState>,
    info: &ErrorInfo,
) -> UpdateEnvelope {
    let mut state = stream.lock();
    state.seq += 1;
    UpdateEnvelope::live(
        instance.clone(),
        0,
        state.session.clone(),
        None,
        state.seq,
        SessionUpdate::Notice {
            level: NoticeLevel::Error,
            text: info.message.clone(),
        },
    )
}

/// Keep the stream complete when the harness fails outside a normal run, so
/// every prompt that ran produces exactly one `TurnEnded` (a prompt rejected
/// as busy gets only an [`error_notice`]):
/// - the run's turn is still open: end it as failed;
/// - the run never started a turn (`turns_before` unchanged): report a
///   started-and-failed turn;
/// - the run's turn already ended (for example the failure was while saving
///   it): report an error notice.
fn close_failed_turn(
    instance: &BackendInstanceId,
    stream: &Mutex<StreamState>,
    turns_before: u64,
    info: &ErrorInfo,
) -> Vec<UpdateEnvelope> {
    let mut state = stream.lock();
    let started = state.mapper.turns_started() > turns_before;
    let mut updates = Vec::new();
    if state.mapper.current_turn().is_some() {
        updates.extend(state.mapper.end_turn_with_error(info.clone()));
    } else if started {
        updates.push(SessionUpdate::Notice {
            level: NoticeLevel::Error,
            text: info.message.clone(),
        });
    } else {
        let turn_id = state.mapper.begin_turn();
        updates.push(SessionUpdate::TurnStarted { turn_id });
        updates.extend(state.mapper.end_turn_with_error(info.clone()));
    }
    let session = state.session.clone();
    updates
        .into_iter()
        .map(|update| {
            let turn = match &update {
                SessionUpdate::TurnStarted { turn_id }
                | SessionUpdate::TurnEnded { turn_id, .. } => Some(turn_id.clone()),
                _ => None,
            };
            state.seq += 1;
            UpdateEnvelope::live(
                instance.clone(),
                0,
                session.clone(),
                turn,
                state.seq,
                update,
            )
        })
        .collect()
}

fn harness_error(err: AgentHarnessError) -> BackendError {
    match err {
        AgentHarnessError::Busy => BackendError::Busy,
        AgentHarnessError::NoModelSelected => {
            BackendError::failed(ErrorCategory::Auth, err.to_string())
        }
        other => BackendError::failed(ErrorCategory::Internal, other.to_string()),
    }
}

fn prompt_input(input: Vec<ContentBlock>) -> BackendResult<PromptInput> {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in input {
        match block {
            ContentBlock::Text { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t);
            }
            ContentBlock::Image { data, mime_type } => {
                images.push(ImageContent { data, mime_type })
            }
            other => {
                return Err(BackendError::Unsupported(format!(
                    "prompt content {}",
                    serde_json::to_value(&other)
                        .ok()
                        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
                        .unwrap_or_else(|| "of unknown type".into())
                )))
            }
        }
    }
    Ok(if images.is_empty() {
        PromptInput::Text(text)
    } else {
        PromptInput::TextWithImages { text, images }
    })
}

#[async_trait]
impl AgentBackend for NativeBackend {
    fn instance(&self) -> &BackendInstanceId {
        &self.instance
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            load_session: true,
            set_model: true,
            efforts: true,
            usage: true,
            images: true,
            ..Capabilities::default()
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<UpdateEnvelope> {
        self.updates.subscribe()
    }

    async fn new_session(&self, spec: SessionSpec) -> BackendResult<SessionRef> {
        let cwd = spec.cwd.map(|p| p.to_string_lossy().into_owned());
        let id = self
            .harness
            .start_new_session(cwd, spec.name)
            .await
            .map_err(harness_error)?;
        let session = self.adopt_session(&id);
        // A new Session starts from the defaults, not the last one's choices.
        let level = *self.default_effort.lock();
        self.harness.set_thinking_level(level).await;
        self.read_config().await;
        Ok(session)
    }

    async fn load_session(
        &self,
        session: &SessionRef,
        _spec: SessionSpec,
    ) -> BackendResult<SessionRef> {
        if session.instance != self.instance {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        let id = self
            .harness
            .switch_session(&session.native_id)
            .await
            .map_err(harness_error)?;
        let session = self.adopt_session(&id);
        self.restore_config().await?;
        let history = self.transcript(&session).await?;
        let mut stream = self.stream.lock();
        for update in history {
            stream.seq += 1;
            let mut envelope = UpdateEnvelope::live(
                self.instance.clone(),
                0,
                session.key.clone(),
                None,
                stream.seq,
                update,
            );
            envelope.origin = Origin::Replay;
            let _ = self.updates.send(envelope);
        }
        Ok(session)
    }

    async fn transcript(&self, session: &SessionRef) -> BackendResult<Vec<SessionUpdate>> {
        self.check_session(session).await?;
        let context = self
            .harness
            .session_context()
            .await
            .map_err(harness_error)?;
        Ok(replay(&context.messages))
    }

    async fn prompt(
        &self,
        session: &SessionRef,
        input: Vec<ContentBlock>,
    ) -> BackendResult<TurnHandle> {
        self.check_session(session).await?;
        // Reserve the turn before checking the harness, so two callers
        // cannot both pass the check.
        if self
            .prompting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(BackendError::Busy);
        }
        let slot = PromptSlot(Arc::clone(&self.prompting));
        if self.harness.phase() != AgentHarnessPhase::Idle {
            return Err(BackendError::Busy);
        }
        let input = prompt_input(input)?;
        let (done, handle) = TurnHandle::channel();
        let harness = Arc::clone(&self.harness);
        let instance = self.instance.clone();
        let stream = Arc::clone(&self.stream);
        let updates = self.updates.clone();
        let turns_before = self.stream.lock().mapper.turns_started();
        let config = Arc::clone(&self.config);
        tokio::spawn(async move {
            let result = harness.prompt(input).await;
            drop(slot);
            let rejected_busy = matches!(result, Err(AgentHarnessError::Busy));
            // A run that returned a message already ended its turn on the
            // stream, even when the message is an error.
            let ran = result.is_ok();
            let outcome = match result {
                Ok(message) => match message.as_assistant().map(turn_outcome) {
                    Some((StopReason::Cancelled, _)) => TurnOutcome::Cancelled,
                    Some((StopReason::Error, error)) => {
                        TurnOutcome::Failed(error.unwrap_or_else(|| {
                            ErrorInfo::new(ErrorCategory::Agent, "the turn failed")
                        }))
                    }
                    Some((stop, _)) => TurnOutcome::Completed(stop),
                    None => TurnOutcome::Completed(StopReason::EndTurn),
                },
                Err(AgentHarnessError::Busy) => TurnOutcome::Failed(ErrorInfo::new(
                    ErrorCategory::Internal,
                    "a turn is already running",
                )),
                Err(err) => TurnOutcome::Failed(match harness_error(err) {
                    BackendError::Failed(info) => info,
                    other => ErrorInfo::new(ErrorCategory::Internal, other.to_string()),
                }),
            };
            match &outcome {
                // Another caller of the harness is running a turn; that
                // turn is not this prompt's to close.
                TurnOutcome::Failed(info) if rejected_busy => {
                    let _ = updates.send(error_notice(&instance, &stream, info));
                }
                TurnOutcome::Failed(info) if !ran => {
                    for envelope in close_failed_turn(&instance, &stream, turns_before, info) {
                        let _ = updates.send(envelope);
                    }
                    apply_pending(&instance, &stream, &config, &updates);
                }
                _ => {}
            }
            let _ = done.send(outcome);
        });
        Ok(handle)
    }

    fn cancel(&self, _session: &SessionRef) {
        self.harness.abort();
    }

    fn is_busy(&self, _session: &SessionRef) -> bool {
        self.is_busy_now()
    }

    async fn shutdown(&self) {
        self.harness.request_shutdown();
    }

    fn compaction(&self) -> Option<&dyn Compaction> {
        Some(self)
    }

    fn session_stats(&self) -> Option<&dyn SessionStats> {
        Some(self)
    }

    fn local_shell(&self) -> Option<&dyn LocalShell> {
        Some(self)
    }

    fn model_control(&self) -> Option<&dyn ModelControl> {
        Some(self)
    }
}

#[async_trait]
impl ModelControl for NativeBackend {
    async fn models(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        self.current_config(session).await?;
        let models = self.harness.models();
        let available = models.get_available().await;
        let state = self.config.lock();
        Ok(ModelChoices {
            current: state.config.model.clone(),
            models: available
                .iter()
                .filter(|m| !models.credentials().is_logged_out(&m.provider))
                .map(|m| ModelChoice {
                    id: model_id(m),
                    name: m.name.clone(),
                    description: None,
                })
                .collect(),
            revision: state.revision,
        })
    }

    async fn set_model(
        &self,
        session: &SessionRef,
        model: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<MutationResult> {
        let _setting = self.setting.lock().await;
        self.current_config(session).await?;
        // Provider ids never contain `/`; model ids may.
        let (provider, id) = model
            .split_once('/')
            .ok_or_else(|| invalid(format!("choose a model as provider/model, not `{model}`")))?;
        if self.harness.models().credentials().is_logged_out(provider) {
            return Err(invalid(format!(
                "{provider} is logged out; run /login {provider}"
            )));
        }
        let resolved = self
            .harness
            .models()
            .get_model(provider, id)
            .ok_or_else(|| invalid(format!("model not found: {provider}/{id}")))?;
        self.check_revision(expected_revision)?;
        self.harness.set_model(resolved).await;
        self.commit(PendingSessionWrite::ModelChange {
            provider: provider.into(),
            model_id: id.into(),
        })
        .await
    }

    async fn efforts(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        self.current_config(session).await?;
        let model = self.harness.model().await;
        let state = self.config.lock();
        Ok(ModelChoices {
            current: Some(effective_effort(model.as_ref(), state.level)),
            models: native_efforts(model.as_ref(), &NATIVE_EFFORTS)
                .into_iter()
                .map(|level| ModelChoice {
                    name: level.clone(),
                    id: level,
                    description: None,
                })
                .collect(),
            revision: state.revision,
        })
    }

    async fn set_effort(
        &self,
        session: &SessionRef,
        effort: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<MutationResult> {
        let _setting = self.setting.lock().await;
        self.current_config(session).await?;
        let level = effort.trim().to_lowercase();
        let model = self.harness.model().await;
        let levels = native_efforts(model.as_ref(), &NATIVE_EFFORTS);
        if !levels.contains(&level) {
            return Err(invalid(format!(
                "unknown effort `{level}`; choose one of: {}",
                levels.join(", ")
            )));
        }
        let thinking_level = parse_thinking(&level);
        self.check_revision(expected_revision)?;
        // A running turn keeps the level it started with.
        self.harness.set_thinking_level(thinking_level).await;
        self.commit(PendingSessionWrite::ThinkingLevelChange { thinking_level })
            .await
    }
}

#[async_trait]
impl Compaction for NativeBackend {
    async fn compact(
        &self,
        session: &SessionRef,
        instructions: Option<&str>,
    ) -> BackendResult<CompactionReport> {
        self.check_session(session).await?;
        let result = self
            .harness
            .compact(instructions)
            .await
            .map_err(harness_error)?;
        Ok(CompactionReport {
            tokens_before: Some(result.tokens_before),
            summary: result.summary,
        })
    }
}

#[async_trait]
impl SessionStats for NativeBackend {
    async fn stats(&self, session: &SessionRef) -> BackendResult<StatsSummary> {
        self.check_session(session).await?;
        let stats = self.harness.session_stats().await.map_err(harness_error)?;
        Ok(StatsSummary {
            total_tokens: stats.tokens.total_tokens(),
            context_tokens: stats.context_usage.as_ref().and_then(|c| c.tokens),
            context_window: stats.context_usage.as_ref().map(|c| c.context_window),
            report: format_session_stats(&stats),
        })
    }
}

#[async_trait]
impl LocalShell for NativeBackend {
    async fn run(&self, session: &SessionRef, command: &str) -> BackendResult<ShellResult> {
        self.check_session(session).await?;
        let env = self.harness.tool_env().await.map_err(harness_error)?;
        let out = env
            .exec(command, ShellExecOptions::default())
            .await
            .map_err(|e| BackendError::failed(ErrorCategory::Agent, e.to_string()))?;
        Ok(ShellResult {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code: out.exit_code,
        })
    }
}
