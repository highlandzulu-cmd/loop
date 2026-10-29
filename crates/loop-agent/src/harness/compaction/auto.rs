//! In-loop compaction: shrink the context between turns of one long agent run.
//!
//! `find_cut_point` snaps to user messages, which cannot split a single long turn (a
//! coding task is typically one user prompt followed by dozens of assistant/tool
//! messages). This planner cuts at an assistant message instead, so retained history
//! never starts with an orphaned tool result.

use futures::StreamExt;
use loop_ai::{
    estimate_context_tokens, estimate_message_tokens, AssistantContent, Context, Message, Model,
    SimpleStreamOptions, ThinkingLevel, ToolResultContent, UserContent, UserMessageContent,
};

use crate::harness::compaction::{should_compact, CompactionSettings};
use crate::messages::convert_to_llm;
use crate::stream_fn::StreamFn;
use crate::types::{AgentContext, AgentMessage};

const SUMMARY_SYSTEM: &str = "You compress the working history of a coding agent so it can \
keep going with a much smaller context and still act exactly as correctly as if it had the full \
history. Write the summary in these sections, in this order.\n\
1. TASK: the user's original request and any required output format, constraints or rules, \
restated exactly.\n\
2. STATE: every number, total, counter, list, maximum or minimum, or other running value the \
agent is tracking, written out exactly as last computed and tagged with the item it was last \
updated for (for example 'running total 337198 after report_45'). Never round, never recompute, \
never omit. Take values from the agent's own messages; do not invent any. If an earlier summary \
appears in the transcript, carry every value in it forward.\n\
3. DONE: files read or changed (and what changed), commands run and their outcomes, test \
results, decisions made. When the task processes many items, list each item's result. Keep \
exact file paths, identifiers and error messages.\n\
4. NEXT: what is left to do and the very next step.\n\
No preamble.";
/// Opening line of the message that carries a summary in the context.
const SUMMARY_MARKER: &str = "[Earlier work was summarized to save context. Continue from here.]";
/// Cap on the summary kept in context.
const MAX_SUMMARY_CHARS: usize = 12_000;
/// Upper cap on the transcript sent to the summarizer; smaller windows get less (see
/// `max_transcript_chars`).
const MAX_TRANSCRIPT_CHARS: usize = 120_000;
/// Output budget for one summary.
const SUMMARY_MAX_OUTPUT_TOKENS: u64 = 3000;

/// Max characters of one tool result / tool-call argument shown to the summarizer.
const RESULT_PREVIEW_CHARS: usize = 1200;
const ARGS_PREVIEW_CHARS: usize = 400;

/// How the context splits when compacting between turns.
#[derive(Debug, Clone)]
pub struct InLoopPlan {
    /// Kept verbatim at the front (the original task prompt).
    pub head: Vec<AgentMessage>,
    /// Older messages to replace with a summary.
    pub to_summarize: Vec<AgentMessage>,
    /// Recent messages kept verbatim; always starts with an assistant message.
    pub retained: Vec<AgentMessage>,
}

fn message_tokens(m: &AgentMessage) -> u64 {
    m.as_llm().map(estimate_message_tokens).unwrap_or(0)
}

/// Split `messages` so roughly `keep_recent_tokens` stay verbatim. Returns `None` when
/// there is nothing worth summarizing (fewer than two messages would be folded away).
pub fn plan_in_loop_compaction(
    messages: &[AgentMessage],
    keep_recent_tokens: u64,
) -> Option<InLoopPlan> {
    let head_len = usize::from(messages.first().is_some_and(|m| m.role() == "user"));

    let mut kept = 0u64;
    let mut cut = None;
    for i in (head_len..messages.len()).rev() {
        kept += message_tokens(&messages[i]);
        if kept >= keep_recent_tokens && messages[i].role() == "assistant" {
            cut = Some(i);
            break;
        }
    }
    let cut = cut?;
    if cut < head_len + 2 {
        return None;
    }
    Some(InLoopPlan {
        head: messages[..head_len].to_vec(),
        to_summarize: messages[head_len..cut].to_vec(),
        retained: messages[cut..].to_vec(),
    })
}

fn preview(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}… [truncated]")
    } else {
        head
    }
}

/// Plain-text transcript of `messages` for the summarizer prompt. Keeps what the
/// summary needs (what was asked, which tools ran with which arguments, how they
/// ended) and drops bulk (full tool output, thinking blocks).
pub fn render_for_summary(messages: &[AgentMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        let Some(msg) = m.as_llm() else { continue };
        match msg {
            Message::User(u) => {
                let text = match &u.content {
                    UserMessageContent::Text(t) => t.clone(),
                    UserMessageContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            UserContent::Text(t) => Some(t.text.as_str()),
                            UserContent::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                // An earlier summary is the only record of the work it replaced: keep it whole.
                let limit = if text.starts_with(SUMMARY_MARKER) {
                    MAX_SUMMARY_CHARS + 500
                } else {
                    2000
                };
                out.push_str(&format!("USER: {}\n", preview(&text, limit)));
            }
            Message::Assistant(a) => {
                for block in &a.content {
                    match block {
                        AssistantContent::Text(t) if !t.text.trim().is_empty() => {
                            out.push_str(&format!("ASSISTANT: {}\n", preview(&t.text, 1200)));
                        }
                        AssistantContent::ToolCall(tc) => {
                            out.push_str(&format!(
                                "CALL {}({})\n",
                                tc.name,
                                preview(&tc.arguments.to_string(), ARGS_PREVIEW_CHARS)
                            ));
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(t) => {
                let text: String = t
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text(t) => Some(t.text.as_str()),
                        ToolResultContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push_str(&format!(
                    "RESULT {}: {}\n",
                    t.tool_name,
                    preview(&text, RESULT_PREVIEW_CHARS)
                ));
            }
        }
    }
    out
}

/// The summary request must fit in the window next to the system prompt and the answer, so
/// small windows get a smaller transcript: about 40% of the window at ~3 characters a token.
fn max_transcript_chars(model: &Model) -> usize {
    if model.context_window == 0 {
        return MAX_TRANSCRIPT_CHARS;
    }
    let chars = (model.context_window as usize).saturating_mul(40) / 100 * 3;
    chars.clamp(8_000, MAX_TRANSCRIPT_CHARS)
}

/// Ask the model for a summary of `transcript`. `None` on any failure or empty reply,
/// so the caller can leave the context untouched instead of losing history.
async fn summarize(
    model: &Model,
    stream_fn: &StreamFn,
    mut options: SimpleStreamOptions,
    transcript: &str,
) -> Option<String> {
    options.reasoning = Some(ThinkingLevel::Minimal);
    options.base.max_tokens = Some(SUMMARY_MAX_OUTPUT_TOKENS as u32);
    let context = Context {
        system_prompt: Some(SUMMARY_SYSTEM.to_string()),
        messages: vec![Message::user_text(format!(
            "Transcript to summarize:\n\n{transcript}"
        ))],
        tools: None,
    };
    let mut stream = stream_fn(model.clone(), context, options).await;
    while stream.next().await.is_some() {}
    let reply = stream.result().await;
    if reply.failure().is_some() {
        return None;
    }
    let text: String = reply
        .content
        .iter()
        .filter_map(|b| match b {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| preview(text, MAX_SUMMARY_CHARS))
}

fn context_tokens(messages: &[AgentMessage]) -> u64 {
    estimate_context_tokens(&Context {
        system_prompt: None,
        messages: convert_to_llm(messages),
        tools: None,
    })
}

/// Between turns: once the context reaches the compaction limit (90% of the model's window by
/// default), older messages are replaced by a model-written summary. The first message (the
/// task) and the newest messages stay verbatim.
///
/// `None` means "leave it alone": below the limit, nothing worth cutting, or the summary
/// call failed (the history is never dropped without a summary).
pub async fn auto_compact_context(
    context: &AgentContext,
    model: &Model,
    stream_fn: &StreamFn,
    summary_options: SimpleStreamOptions,
    settings: &CompactionSettings,
) -> Option<AutoCompaction> {
    compact_context(context, model, stream_fn, summary_options, settings, false).await
}

/// Same as [`auto_compact_context`] but without waiting for the trigger: used when the provider
/// has just rejected a request as too long. `None` when compaction is disabled or nothing can
/// be shrunk.
pub async fn force_compact_context(
    context: &AgentContext,
    model: &Model,
    stream_fn: &StreamFn,
    summary_options: SimpleStreamOptions,
    settings: &CompactionSettings,
) -> Option<AutoCompaction> {
    if !settings.enabled {
        return None;
    }
    compact_context(context, model, stream_fn, summary_options, settings, true).await
}

/// A compacted context plus a one-line description for the user.
pub struct AutoCompaction {
    /// Context for the next request.
    pub context: AgentContext,
    /// For example "Context compacted: 118,092 → 22,990 tokens".
    pub notice: String,
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

async fn compact_context(
    context: &AgentContext,
    model: &Model,
    stream_fn: &StreamFn,
    summary_options: SimpleStreamOptions,
    settings: &CompactionSettings,
    force: bool,
) -> Option<AutoCompaction> {
    let tokens_before = context_tokens(&context.messages);
    if !force && !should_compact(tokens_before, model.context_window, settings) {
        return None;
    }
    let limit = super::compaction_limit(model.context_window, settings);
    let mut keep = super::effective_keep_recent_tokens(limit, settings.keep_recent_tokens);
    if force {
        // The provider thinks the context is too big whatever we estimated: keep less.
        keep = keep.min((tokens_before / 3).max(1));
    }
    let plan = plan_in_loop_compaction(&context.messages, keep)?;

    let mut transcript = render_for_summary(&plan.to_summarize);
    let max_chars = max_transcript_chars(model);
    if transcript.len() > max_chars {
        // Keep the most recent part: it is closest to what the agent is doing now.
        let start = transcript.len() - max_chars;
        let start = (start..transcript.len())
            .find(|i| transcript.is_char_boundary(*i))
            .unwrap_or(start);
        transcript = format!("[earlier part omitted]\n{}", &transcript[start..]);
    }
    let summary = summarize(model, stream_fn, summary_options, &transcript).await?;

    let mut out = plan.head;
    out.push(AgentMessage::user_text(format!(
        "{SUMMARY_MARKER}\n\n{summary}"
    )));
    out.extend(plan.retained);
    let tokens_after: u64 = out.iter().map(message_tokens).sum();

    tracing::info!(
        tokens_before,
        tokens_after,
        messages_before = context.messages.len(),
        messages_after = out.len(),
        "auto-compacted context between turns"
    );
    Some(AutoCompaction {
        context: AgentContext {
            system_prompt: context.system_prompt.clone(),
            messages: out,
            tools: context.tools.clone(),
        },
        notice: format!(
            "Context compacted automatically: {} → {} tokens (older work summarized)",
            thousands(tokens_before),
            thousands(tokens_after)
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_ai::{AssistantMessage, TextContent};

    fn assistant(text: &str) -> AgentMessage {
        let model = loop_ai::providers::soket_seed_models().remove(0);
        let mut a = AssistantMessage::pending(&model);
        a.content = vec![AssistantContent::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })];
        AgentMessage::assistant(a)
    }

    fn long_history(n: usize) -> Vec<AgentMessage> {
        let mut v = vec![AgentMessage::user_text("task prompt")];
        for i in 0..n {
            v.push(assistant(&format!("step {i} {}", "x".repeat(400))));
        }
        v
    }

    #[test]
    fn splits_a_single_long_turn_at_an_assistant_message() {
        let msgs = long_history(30);
        let plan = plan_in_loop_compaction(&msgs, 300).expect("should plan");
        assert_eq!(plan.head.len(), 1);
        assert_eq!(plan.head[0].role(), "user");
        assert_eq!(plan.retained.first().map(|m| m.role()), Some("assistant"));
        assert_eq!(
            plan.head.len() + plan.to_summarize.len() + plan.retained.len(),
            msgs.len()
        );
        assert!(plan.to_summarize.len() >= 2);
    }

    #[test]
    fn nothing_to_do_for_short_history() {
        assert!(plan_in_loop_compaction(&long_history(2), 10_000).is_none());
        assert!(plan_in_loop_compaction(&[], 100).is_none());
    }

    #[test]
    fn earlier_summary_is_kept_whole_when_summarizing_again() {
        let long = "k".repeat(5000);
        let msgs = vec![
            AgentMessage::user_text("fix it"),
            AgentMessage::user_text(format!("{SUMMARY_MARKER}\n\nSTATE: {long}")),
        ];
        let text = render_for_summary(&msgs);
        assert!(
            text.contains(&long),
            "a prior summary must not be cut to 2000 chars"
        );
        assert!(!text.contains("[truncated]"));
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(118_092), "118,092");
        assert_eq!(thousands(1_048_576), "1,048,576");
    }

    #[test]
    fn transcript_cap_scales_with_the_window() {
        let mut model = loop_ai::providers::soket_seed_models().remove(0);
        model.context_window = 1_048_576;
        assert_eq!(max_transcript_chars(&model), MAX_TRANSCRIPT_CHARS);
        model.context_window = 32_768;
        assert_eq!(max_transcript_chars(&model), 32_768 * 40 / 100 * 3);
        model.context_window = 2_000;
        assert_eq!(max_transcript_chars(&model), 8_000);
        model.context_window = 0;
        assert_eq!(max_transcript_chars(&model), MAX_TRANSCRIPT_CHARS);
    }

    #[test]
    fn render_truncates_bulk() {
        let msgs = vec![
            AgentMessage::user_text("fix it"),
            assistant(&"y".repeat(5000)),
        ];
        let text = render_for_summary(&msgs);
        assert!(text.contains("USER: fix it"));
        assert!(text.contains("[truncated]"));
        assert!(text.len() < 2000);
    }
}
