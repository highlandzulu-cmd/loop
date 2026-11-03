//! `/usage` inside the chat. While open, the usage view owns the screen and the keys;
//! the chat loop keeps draining agent events and hands it a fresh live status, so turns
//! and sub-agents carry on and the **Now** view stays current. Nothing runs while closed.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event;
use loop_dashboard::{
    Dashboard, LiveStatus, McpServerStatus, Options, Palette, SubAgent, SubAgentState,
};

use super::{CardStatus, ChatItem, SurfaceClient, Theme};

/// How often the Now view is rebuilt from the chat's in-memory state.
const LIVE_EVERY: Duration = Duration::from_millis(500);
/// How often sandbox and MCP state are queried again and this session's spans written to
/// the usage log.
const SLOW_EVERY: Duration = Duration::from_secs(5);

/// Usage view settings for the log in `dir`, in the colors of the chat theme. `notice`
/// is shown above the views.
pub fn options(theme: &Theme, dir: PathBuf, notice: Option<String>) -> Options {
    Options {
        dir,
        palette: Palette {
            text: theme.get("text"),
            muted: theme.get("muted"),
            dim: theme.get("dim"),
            border: theme.get("borderMuted"),
            accent: theme.get("borderAccent"),
            series_1: theme.get("borderAccent"),
            series_2: theme.get("warning"),
            good: theme.get("success"),
            selected_bg: theme.get("selectedBg"),
        },
        notice,
        live: None,
    }
}

/// Finished Subagents shown in the Now view; running ones are always shown.
const FINISHED_SHOWN: usize = 5;

/// What the chat is doing right now.
pub(super) struct ChatState<'a> {
    pub items: &'a [ChatItem],
    pub working: bool,
    pub turn_step: &'a str,
}

/// The open dashboard.
pub(super) struct Screen {
    board: Dashboard,
    size: Option<(u16, u16)>,
    slow: Slow,
    live_at: Instant,
    slow_at: Instant,
    period: Option<u32>,
}

impl Screen {
    pub async fn open(runtime: &mut SurfaceClient, chat: ChatState<'_>) -> io::Result<Self> {
        let slow = Slow::query(runtime).await;
        let summary = runtime
            .usage_history(Some(30))
            .await
            .ok()
            .and_then(|r| r.summary)
            .and_then(|s| serde_json::from_value(s).ok());
        let rows = runtime.loaded_sessions().await.unwrap_or_default();
        let mut options = options(runtime.theme(), PathBuf::new(), None);
        options.live = Some(live_status(runtime, &chat, &slow));
        let board = Dashboard::open_served(options, summary, rows)?;
        Ok(Self {
            board,
            size: crossterm::terminal::size().ok(),
            slow,
            live_at: Instant::now(),
            slow_at: Instant::now(),
            period: Some(30),
        })
    }

    /// Wait up to `wait` for keys and pass them on; returns `true` once the user closed it.
    pub fn handle_input(&mut self, wait: Duration) -> io::Result<bool> {
        let mut ready = event::poll(wait)?;
        while ready {
            if self.board.handle_event(&event::read()?)? {
                return Ok(true);
            }
            ready = event::poll(Duration::ZERO)?;
        }
        Ok(false)
    }

    /// Refresh the live status when due and redraw.
    pub async fn refresh(
        &mut self,
        runtime: &mut SurfaceClient,
        chat: ChatState<'_>,
    ) -> io::Result<()> {
        let period = self.board.period_days();
        if self.slow_at.elapsed() >= SLOW_EVERY || self.period != period {
            self.slow = Slow::query(runtime).await;
            let summary = runtime
                .usage_history(period)
                .await
                .ok()
                .and_then(|r| r.summary)
                .and_then(|s| serde_json::from_value(s).ok());
            let rows = runtime.loaded_sessions().await.unwrap_or_default();
            self.board.set_host_data(summary, rows);
            self.period = period;
            self.slow_at = Instant::now();
        }
        if self.live_at.elapsed() >= LIVE_EVERY {
            self.slow.stats = runtime.stats().await.ok();
            self.board.set_live(live_status(runtime, &chat, &self.slow));
            self.live_at = Instant::now();
        }
        self.board.tick()
    }

    /// Back to the chat; returns the terminal width if it was resized meanwhile.
    pub fn close(self) -> io::Result<Option<u16>> {
        self.board.close()?;
        Ok(crossterm::terminal::size()
            .ok()
            .filter(|size| Some(*size) != self.size)
            .map(|(width, _)| width))
    }
}

/// Status that needs a query and seldom changes.
struct Slow {
    sandbox: String,
    mcp_servers: Vec<McpServerStatus>,
    stats: Option<loop_protocol::StatsSummary>,
}
impl Slow {
    async fn query(runtime: &mut SurfaceClient) -> Self {
        let sandbox = runtime
            .sandbox_service()
            .sandbox(loop_protocol::admin::SandboxParams {
                session: runtime.session_ref(),
                action: loop_protocol::admin::SandboxAction::Status,
                config: None,
                expected_revision: None,
            })
            .await
            .map(|s| s.info)
            .unwrap_or_default();
        let mcp_servers = runtime
            .mcp(loop_protocol::admin::AdminAction::List, None)
            .await
            .map(|r| {
                r.servers
                    .into_iter()
                    .filter(|s| s.enabled)
                    .map(|s| McpServerStatus {
                        name: s.name,
                        tools: s.tools as usize,
                        connected: s.connected,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            sandbox,
            mcp_servers,
            stats: runtime.stats().await.ok(),
        }
    }
}

/// The Now view's status, from in-memory state only.
fn live_status(runtime: &SurfaceClient, chat: &ChatState<'_>, slow: &Slow) -> LiveStatus {
    let subscription = runtime
        .active_agent()
        .as_deref()
        .map(|id| runtime.agent_label(id).unwrap_or_else(|| id.to_string()));
    let (model, effort) = match &subscription {
        Some(agent) => (
            runtime
                .agent_model()
                .as_ref()
                .map_or(agent.clone(), |m| format!("{agent}/{m}")),
            runtime.agent_effort().clone(),
        ),
        None => (
            runtime
                .selected_model_spec()
                .unwrap_or_else(|| "none selected".into()),
            Some(runtime.native_effort()).filter(|e| e != "off"),
        ),
    };
    let sub_agents = chat
        .items
        .iter()
        .filter_map(|item| match item {
            ChatItem::WorkflowTask {
                task_id,
                description,
                status,
                ..
            } => Some(SubAgent {
                id: task_id.clone(),
                description: description.clone(),
                state: match status {
                    CardStatus::Pending => SubAgentState::Running,
                    CardStatus::Success | CardStatus::Error => SubAgentState::Finished,
                },
            }),
            _ => None,
        })
        .collect();
    let sub_agents = recent_sub_agents(sub_agents);
    LiveStatus {
        taken_at: chrono::Local::now(),
        model,
        effort,
        permission_mode: runtime.permission_mode().label().to_string(),
        sandbox: slow.sandbox.clone(),
        web: runtime.web_label(),
        cwd: runtime.cwd().display().to_string(),
        session_id: runtime.session_id().clone(),
        context_tokens: slow.stats.as_ref().and_then(|s| s.context_tokens),
        context_window: slow
            .stats
            .as_ref()
            .and_then(|s| s.context_window)
            .unwrap_or(0),
        session_tokens: slow.stats.as_ref().map_or(0, |s| s.total_tokens),
        main_agent_working: chat.working,
        main_agent_step: if chat.working {
            chat.turn_step.to_string()
        } else {
            String::new()
        },
        external_agent: subscription.map(|agent| format!("{agent} · subscription agent")),
        sub_agents,
        skills_loaded: runtime
            .resource_view()
            .skills
            .into_iter()
            .filter(|s| s.enabled)
            .map(|s| s.name)
            .collect(),
        skills_active: runtime.resource_view().active_skills,
        skills_disabled: runtime
            .resource_view()
            .skills
            .iter()
            .filter(|s| !s.enabled)
            .count(),
        mcp_servers: slow.mcp_servers.clone(),
        extensions: runtime.resource_view().extensions,
        hooks: runtime.resource_view().hooks,
        usage_log: runtime.usage_log_status().map(|s| s.0),
    }
}

/// Running Subagents plus the last [`FINISHED_SHOWN`] finished ones, in transcript order.
fn recent_sub_agents(all: Vec<SubAgent>) -> Vec<SubAgent> {
    let finished = all
        .iter()
        .filter(|agent| agent.state == SubAgentState::Finished)
        .count();
    let mut skip = finished.saturating_sub(FINISHED_SHOWN);
    all.into_iter()
        .filter(|agent| {
            let drop = agent.state == SubAgentState::Finished && skip > 0;
            skip -= usize::from(drop);
            !drop
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_view_keeps_running_and_the_last_finished_subagents() {
        let agent = |id: usize, state| SubAgent {
            id: id.to_string(),
            description: String::new(),
            state,
        };
        let mut all: Vec<_> = (0..8)
            .map(|id| agent(id, SubAgentState::Finished))
            .collect();
        all.insert(1, agent(100, SubAgentState::Running));
        let ids: Vec<_> = recent_sub_agents(all)
            .into_iter()
            .map(|agent| agent.id)
            .collect();
        assert_eq!(ids, ["100", "3", "4", "5", "6", "7"]);
    }
}

#[cfg(test)]
mod session_stats_tests {
    use super::*;
    #[tokio::test]
    async fn dashboard_uses_attached_session_stats_and_lists_only_loaded_sessions() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            let mut runtime = crate::surface_client::test_support::fixture(dir.path()).await;
            runtime.new_session().await.unwrap();
            let slow = Slow {
                sandbox: "off".into(),
                mcp_servers: vec![],
                stats: Some(loop_protocol::StatsSummary {
                    total_tokens: 321,
                    context_tokens: Some(123),
                    context_window: Some(4096),
                    ..Default::default()
                }),
            };
            let state = live_status(
                &runtime,
                &ChatState {
                    items: &[],
                    working: false,
                    turn_step: "",
                },
                &slow,
            );
            assert_eq!(
                (
                    state.session_tokens,
                    state.context_tokens,
                    state.context_window
                ),
                (321, Some(123), 4096)
            );
            let loaded = runtime.loaded_sessions().await.unwrap();
            assert_eq!(loaded.totals.sessions, 1);
            assert_eq!(loaded.sessions[0].session, runtime.session_ref());
            let history = runtime.usage_history(None).await.unwrap();
            assert!(history.summary.is_some());
        })
        .await
        .unwrap();
    }
}
