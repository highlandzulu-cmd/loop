//! Reasoning effort levels of Loop's own agent, as `/effort` and
//! `session/efforts` name them.

use crate::types::AgentThinkingLevel;

/// Every effort level, in order.
pub const NATIVE_EFFORTS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The levels Shift+Tab cycles through.
pub const CYCLE_EFFORTS: [&str; 4] = ["off", "low", "medium", "high"];

/// The levels of `candidates` that `model` accepts: only `off` for a model
/// without reasoning, and none its thinking map marks unsupported.
pub fn native_efforts(model: Option<&loop_ai::Model>, candidates: &[&str]) -> Vec<String> {
    candidates
        .iter()
        .filter(|level| {
            let Some(model) = model else {
                return true;
            };
            let Some(reasoning) = parse_thinking(level).to_reasoning() else {
                return true; // "off" is always available
            };
            model.reasoning
                && !loop_ai::reasoning_rejected(model)
                && model.thinking_level_map.as_ref().is_none_or(|map| {
                    map.get(&loop_ai::ModelThinkingLevel::from(reasoning)) != Some(&None)
                })
        })
        .map(|level| (*level).to_owned())
        .collect()
}

/// The level `model` runs `level` at: `level`, or `off` when the model does
/// not accept it.
pub fn effective_effort(model: Option<&loop_ai::Model>, level: AgentThinkingLevel) -> String {
    let label = thinking_label(level);
    if native_efforts(model, &NATIVE_EFFORTS)
        .iter()
        .any(|l| l == label)
    {
        label.into()
    } else {
        "off".into()
    }
}

/// The level named `s`; unknown names are `off`.
pub fn parse_thinking(s: &str) -> AgentThinkingLevel {
    match s.to_lowercase().as_str() {
        "minimal" => AgentThinkingLevel::Minimal,
        "low" => AgentThinkingLevel::Low,
        "medium" => AgentThinkingLevel::Medium,
        "high" => AgentThinkingLevel::High,
        "xhigh" => AgentThinkingLevel::XHigh,
        "max" => AgentThinkingLevel::Max,
        _ => AgentThinkingLevel::Off,
    }
}

/// The name of `level`.
pub fn thinking_label(level: AgentThinkingLevel) -> &'static str {
    match level {
        AgentThinkingLevel::Off => "off",
        AgentThinkingLevel::Minimal => "minimal",
        AgentThinkingLevel::Low => "low",
        AgentThinkingLevel::Medium => "medium",
        AgentThinkingLevel::High => "high",
        AgentThinkingLevel::XHigh => "xhigh",
        AgentThinkingLevel::Max => "max",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_efforts_follow_the_model() {
        let all = ["off", "low", "high"];
        let mut model = loop_ai::providers::soket_seed_models().remove(0);
        assert_eq!(native_efforts(Some(&model), &all), ["off", "low", "high"]);

        model.thinking_level_map = Some(std::collections::HashMap::from([(
            loop_ai::ModelThinkingLevel::High,
            None,
        )]));
        assert_eq!(native_efforts(Some(&model), &all), ["off", "low"]);
        assert_eq!(
            effective_effort(Some(&model), AgentThinkingLevel::High),
            "off"
        );
        assert_eq!(
            effective_effort(Some(&model), AgentThinkingLevel::Low),
            "low"
        );

        model.reasoning = false;
        assert_eq!(native_efforts(Some(&model), &all), ["off"]);
        assert_eq!(native_efforts(None, &all), ["off", "low", "high"]);
    }

    #[test]
    fn labels_round_trip() {
        for label in NATIVE_EFFORTS {
            assert_eq!(thinking_label(parse_thinking(label)), label);
        }
    }
}
