//! Auto-detect OpenAI Completions compatibility flags from a base URL.

use crate::types::{
    MaxTokensField, OpenAICompletionsCompat, ResolvedOpenAICompletionsCompat, ThinkingFormat,
};

/// Infer sensible compat defaults from `base_url`.
pub fn detect_compat(base_url: &str) -> OpenAICompletionsCompat {
    let url = base_url.to_lowercase();
    let is_local = url.contains("localhost")
        || url.contains("127.0.0.1")
        || url.contains("0.0.0.0")
        || url.contains("[::1]");
    let is_openai = url.contains("api.openai.com");
    let is_openrouter = url.contains("openrouter.ai");
    let is_deepseek = url.contains("deepseek");
    let is_together = url.contains("together");
    let is_groq = url.contains("groq.com");
    // Soket's endpoint accepts the `metadata` object (session affinity);
    // everywhere else it is an undocumented field that strict servers
    // (e.g. Z.ai, error 1210) reject outright.
    // Matched on the parsed host, not as a substring of the URL: a host such
    // as `api.xyz.ai` must not be taken for `z.ai`.
    let host = url_host(base_url);
    let host = host.as_deref().unwrap_or("");
    // FIXME(confirm): the `tensorstudio` match predates host matching and is
    // kept as is; confirm its real domain before it is changed or removed.
    let is_soket =
        host_is(host, "soket.ai") || host.split('.').any(|label| label == "tensorstudio");
    let is_zai = host_is(host, "z.ai") || host_is(host, "bigmodel.cn");
    // `metadata` is only sent where it is known to be accepted.
    let supports_metadata = Some(is_soket);

    if is_local {
        return OpenAICompletionsCompat {
            supports_store: Some(false),
            supports_metadata,
            supports_developer_role: Some(false),
            supports_reasoning_effort: Some(false),
            supports_usage_in_streaming: Some(true),
            supports_finish_reason: Some(true),
            max_tokens_field: Some(MaxTokensField::MaxTokens),
            requires_tool_result_name: Some(false),
            requires_assistant_after_tool_result: Some(false),
            requires_thinking_as_text: Some(false),
            thinking_format: Some(ThinkingFormat::Openai),
            session_affinity_format: None,
        };
    }

    if is_openai {
        return OpenAICompletionsCompat {
            supports_store: Some(true),
            supports_metadata,
            supports_developer_role: Some(true),
            supports_reasoning_effort: Some(true),
            supports_usage_in_streaming: Some(true),
            supports_finish_reason: Some(true),
            max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
            requires_tool_result_name: Some(false),
            requires_assistant_after_tool_result: Some(false),
            requires_thinking_as_text: Some(false),
            thinking_format: Some(ThinkingFormat::Openai),
            session_affinity_format: None,
        };
    }

    let thinking_format = if is_openrouter {
        ThinkingFormat::Openrouter
    } else if is_zai {
        ThinkingFormat::Zai
    } else if is_deepseek {
        ThinkingFormat::Deepseek
    } else if is_together {
        ThinkingFormat::Together
    } else {
        ThinkingFormat::Openai
    };

    OpenAICompletionsCompat {
        supports_store: Some(false),
        supports_metadata,
        supports_developer_role: Some(false),
        supports_reasoning_effort: Some(is_openai || is_openrouter || is_deepseek),
        supports_usage_in_streaming: Some(true),
        supports_finish_reason: Some(true),
        max_tokens_field: Some(if is_groq {
            MaxTokensField::MaxTokens
        } else {
            MaxTokensField::MaxCompletionTokens
        }),
        requires_tool_result_name: Some(false),
        requires_assistant_after_tool_result: Some(false),
        requires_thinking_as_text: Some(false),
        thinking_format: Some(thinking_format),
        session_affinity_format: None,
    }
}

/// Lowercase host of `base_url`; a missing scheme is tolerated.
fn url_host(base_url: &str) -> Option<String> {
    let base_url = base_url.trim();
    let parsed = reqwest::Url::parse(base_url)
        .ok()
        .filter(|url| url.host_str().is_some())
        .or_else(|| reqwest::Url::parse(&format!("https://{base_url}")).ok())?;
    parsed.host_str().map(|host| host.to_ascii_lowercase())
}

/// Whether `host` is `domain` or a subdomain of it.
fn host_is(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|rest| rest.ends_with('.'))
}

/// Merge URL-detected defaults with explicit model overrides.
pub fn resolve_compat(
    model_base_url: &str,
    override_compat: Option<&OpenAICompletionsCompat>,
) -> ResolvedOpenAICompletionsCompat {
    let detected = detect_compat(model_base_url);
    let o = override_compat.cloned().unwrap_or_default();

    ResolvedOpenAICompletionsCompat {
        supports_store: o
            .supports_store
            .or(detected.supports_store)
            .unwrap_or(false),
        supports_metadata: o
            .supports_metadata
            .or(detected.supports_metadata)
            .unwrap_or(false),
        supports_developer_role: o
            .supports_developer_role
            .or(detected.supports_developer_role)
            .unwrap_or(false),
        supports_reasoning_effort: o
            .supports_reasoning_effort
            .or(detected.supports_reasoning_effort)
            .unwrap_or(false),
        supports_usage_in_streaming: o
            .supports_usage_in_streaming
            .or(detected.supports_usage_in_streaming)
            .unwrap_or(true),
        supports_finish_reason: o
            .supports_finish_reason
            .or(detected.supports_finish_reason)
            .unwrap_or(true),
        max_tokens_field: o
            .max_tokens_field
            .or(detected.max_tokens_field)
            .unwrap_or(MaxTokensField::MaxTokens),
        requires_tool_result_name: o
            .requires_tool_result_name
            .or(detected.requires_tool_result_name)
            .unwrap_or(false),
        requires_assistant_after_tool_result: o
            .requires_assistant_after_tool_result
            .or(detected.requires_assistant_after_tool_result)
            .unwrap_or(false),
        requires_thinking_as_text: o
            .requires_thinking_as_text
            .or(detected.requires_thinking_as_text)
            .unwrap_or(false),
        thinking_format: o
            .thinking_format
            .or(detected.thinking_format)
            .unwrap_or(ThinkingFormat::Openai),
        session_affinity_format: o
            .session_affinity_format
            .or(detected.session_affinity_format),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localhost_disables_openai_specifics() {
        let c = detect_compat("http://localhost:11434/v1");
        assert_eq!(c.supports_store, Some(false));
        assert_eq!(c.supports_developer_role, Some(false));
        assert_eq!(c.max_tokens_field, Some(MaxTokensField::MaxTokens));
    }

    #[test]
    fn openai_cloud_defaults() {
        let c = detect_compat("https://api.openai.com/v1");
        assert_eq!(c.supports_developer_role, Some(true));
        assert_eq!(
            c.max_tokens_field,
            Some(MaxTokensField::MaxCompletionTokens)
        );
    }

    #[test]
    fn override_wins() {
        let resolved = resolve_compat(
            "http://localhost:8080/v1",
            Some(&OpenAICompletionsCompat {
                supports_store: Some(true),
                ..Default::default()
            }),
        );
        assert!(resolved.supports_store);
        assert!(!resolved.supports_developer_role);
    }

    #[test]
    fn metadata_only_for_soket() {
        // Soket's endpoint accepts the `metadata` object.
        assert_eq!(
            detect_compat("https://api.soket.ai/v1").supports_metadata,
            Some(true)
        );
        // Everything else — including Z.ai, which rejects unknown fields
        // with HTTP 400 "Invalid API parameter" — defaults to not sending it.
        for url in [
            "https://api.z.ai/api/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
            "https://api.openai.com/v1",
            "https://openrouter.ai/api/v1",
            "http://localhost:11434/v1",
        ] {
            assert_eq!(
                detect_compat(url).supports_metadata,
                Some(false),
                "metadata should be off for {url}"
            );
            assert!(!resolve_compat(url, None).supports_metadata);
        }
        // An explicit override can re-enable it (self-hosted gateways).
        assert!(
            resolve_compat(
                "https://api.z.ai/api/paas/v4",
                Some(&OpenAICompletionsCompat {
                    supports_metadata: Some(true),
                    ..Default::default()
                })
            )
            .supports_metadata
        );
    }

    #[test]
    fn supports_metadata_reads_from_model_config() {
        let o: OpenAICompletionsCompat =
            serde_json::from_str(r#"{"supportsMetadata": true}"#).unwrap();
        assert_eq!(o.supports_metadata, Some(true));
        assert!(resolve_compat("https://gateway.example.com/v1", Some(&o)).supports_metadata);
    }

    #[test]
    fn zai_uses_the_zai_thinking_format() {
        assert_eq!(
            detect_compat("https://api.z.ai/api/paas/v4").thinking_format,
            Some(ThinkingFormat::Zai)
        );
        assert_eq!(
            detect_compat("https://open.bigmodel.cn/api/paas/v4").thinking_format,
            Some(ThinkingFormat::Zai)
        );
    }

    #[test]
    fn provider_hosts_match_on_the_host_not_a_substring() {
        for url in [
            "https://z.ai/v1",
            "https://api.z.ai/api/paas/v4",
            "https://open.bigmodel.cn/api/paas/v4",
            "https://bigmodel.cn/v1",
            "https://API.Z.AI/v1",
            "https://api.z.ai:8443/v1",
            "api.z.ai/v1",
        ] {
            assert_eq!(
                detect_compat(url).thinking_format,
                Some(ThinkingFormat::Zai),
                "{url} should be Z.ai"
            );
        }
        for url in [
            "https://api.xyz.ai/v1",
            "https://notz.ai/v1",
            "https://z.ai.evil.com/v1",
            "https://evil.com/z.ai/v1",
            "https://evil.com/?u=bigmodel.cn",
            "https://api.z.ai@evil.com/v1",
            "https://mybigmodel.cn/v1",
            "https://bigmodel.example.com/v1",
        ] {
            assert_eq!(
                detect_compat(url).thinking_format,
                Some(ThinkingFormat::Openai),
                "{url} must not be Z.ai"
            );
        }
        for url in [
            "https://soket.ai/v1",
            "https://api.soket.ai/v1",
            "https://tensorstudio.ai/v1",
            "https://api.tensorstudio.com/v1",
        ] {
            assert_eq!(
                detect_compat(url).supports_metadata,
                Some(true),
                "{url} should accept metadata"
            );
        }
        for url in [
            "https://notsoket.ai/v1",
            "https://soket.ai.evil.com/v1",
            "https://evil.com/soket.ai/v1",
            "https://nottensorstudio.com/v1",
        ] {
            assert_eq!(
                detect_compat(url).supports_metadata,
                Some(false),
                "{url} must not accept metadata"
            );
        }
    }
}
