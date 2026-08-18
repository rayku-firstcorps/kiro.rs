//! Local compatibility layer for Anthropic prompt caching.
//!
//! Kiro does not expose a prompt-cache API. We still track Anthropic cache
//! breakpoints locally so clients such as NewAPI receive useful cache usage
//! accounting. This does not reduce the request sent to Kiro.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::types::{CacheControl, ContentBlock, MessagesRequest};
use crate::token;

const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheUsage {
    pub cache_creation_input_tokens: i32,
    pub cache_read_input_tokens: i32,
    pub cache_creation_5m_input_tokens: i32,
    pub cache_creation_1h_input_tokens: i32,
}

impl CacheUsage {
    pub fn bounded(self, total_input_tokens: i32) -> Self {
        let total = total_input_tokens.max(0);
        let cache_read_input_tokens = self.cache_read_input_tokens.min(total);
        let cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .min(total.saturating_sub(cache_read_input_tokens));
        let cache_creation_5m_input_tokens = self
            .cache_creation_5m_input_tokens
            .min(cache_creation_input_tokens);
        let cache_creation_1h_input_tokens = self
            .cache_creation_1h_input_tokens
            .min(cache_creation_input_tokens.saturating_sub(cache_creation_5m_input_tokens));
        Self {
            cache_creation_input_tokens,
            cache_read_input_tokens,
            cache_creation_5m_input_tokens,
            cache_creation_1h_input_tokens,
        }
    }

    pub fn uncached_input_tokens(self, total_input_tokens: i32) -> i32 {
        total_input_tokens
            .saturating_sub(self.cache_creation_input_tokens)
            .saturating_sub(self.cache_read_input_tokens)
            .max(0)
    }
}

#[derive(Debug)]
struct CacheEntry {
    expires_at: Instant,
    input_tokens: i32,
}

static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();

/// Record Anthropic cache breakpoints in a request.
pub fn record_request(req: &MessagesRequest) -> CacheUsage {
    let candidates = cache_prefixes(req);
    if candidates.is_empty() {
        return CacheUsage::default();
    }
    let candidates: Vec<_> = candidates
        .into_iter()
        .map(|(prefix, ttl, input_tokens)| {
            let prefix_json = serde_json::to_vec(&prefix).unwrap_or_default();
            let mut hasher = Sha256::new();
            hasher.update(req.model.as_bytes());
            hasher.update([0]);
            hasher.update(&prefix_json);
            (format!("{:x}", hasher.finalize()), ttl, input_tokens)
        })
        .collect();

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    let mut entries = cache.lock().expect("prompt cache lock poisoned");
    entries.retain(|_, entry| entry.expires_at > now);

    let cache_read_input_tokens = candidates
        .iter()
        .rev()
        .find_map(|(key, _, _)| entries.get(key).map(|entry| entry.input_tokens))
        .unwrap_or(0);
    let mut accounted_input_tokens = cache_read_input_tokens;
    let mut cache_creation_5m_input_tokens = 0_i32;
    let mut cache_creation_1h_input_tokens = 0_i32;

    for (key, ttl, input_tokens) in candidates {
        if !entries.contains_key(&key) && input_tokens > accounted_input_tokens {
            let created = input_tokens.saturating_sub(accounted_input_tokens);
            if ttl >= Duration::from_secs(60 * 60) {
                cache_creation_1h_input_tokens =
                    cache_creation_1h_input_tokens.saturating_add(created);
            } else {
                cache_creation_5m_input_tokens =
                    cache_creation_5m_input_tokens.saturating_add(created);
            }
            accounted_input_tokens = input_tokens;
        }
        entries.entry(key).or_insert(CacheEntry {
            expires_at: now + ttl,
            input_tokens,
        });
    }
    let cache_creation_input_tokens =
        cache_creation_5m_input_tokens.saturating_add(cache_creation_1h_input_tokens);
    tracing::debug!(
        cache_creation_input_tokens,
        cache_read_input_tokens,
        "更新本地 Anthropic 提示词缓存"
    );
    CacheUsage {
        cache_creation_input_tokens,
        cache_read_input_tokens,
        cache_creation_5m_input_tokens,
        cache_creation_1h_input_tokens,
    }
}

/// Return every request prefix ending at an Anthropic cache breakpoint.
fn cache_prefixes(req: &MessagesRequest) -> Vec<(Vec<Value>, Duration, i32)> {
    let mut segments = Vec::new();
    let mut token_counts = Vec::new();
    let mut breakpoints: Vec<(usize, Duration)> = Vec::new();

    if let Some(system) = &req.system {
        for message in system {
            segments.push(json!({"role": "system", "text": message.text}));
            token_counts.push(count_text(&message.text));
            if let Some(control) = &message.cache_control {
                breakpoints.push((segments.len() - 1, ttl(control)));
            }
        }
    }

    if let Some(tools) = &req.tools {
        for tool in tools {
            segments.push(json!({"role": "tool", "tool": tool}));
            let schema = serde_json::to_string(&tool.input_schema).unwrap_or_default();
            token_counts.push(
                count_text(&tool.name)
                    .saturating_add(count_text(&tool.description))
                    .saturating_add(count_text(&schema)),
            );
            if let Some(control) = &tool.cache_control {
                breakpoints.push((segments.len() - 1, ttl(control)));
            }
        }
    }

    for message in &req.messages {
        match &message.content {
            Value::String(text) => {
                segments.push(json!({"role": message.role, "text": text}));
                token_counts.push(count_text(text));
            }
            Value::Array(blocks) => {
                for block in blocks {
                    segments.push(json!({"role": message.role, "content": block}));
                    token_counts.push(content_block_tokens(block));
                    if let Ok(block) = serde_json::from_value::<ContentBlock>(block.clone())
                        && let Some(control) = block.cache_control
                    {
                        breakpoints.push((segments.len() - 1, ttl(&control)));
                    }
                }
            }
            other => {
                segments.push(json!({"role": message.role, "content": other}));
                token_counts.push(0);
            }
        }
    }

    breakpoints
        .into_iter()
        .map(|(index, ttl)| {
            let input_tokens = token_counts[..=index]
                .iter()
                .copied()
                .fold(0_i32, i32::saturating_add)
                .max(1);
            (segments[..=index].to_vec(), ttl, input_tokens)
        })
        .collect()
}

fn count_text(text: &str) -> i32 {
    token::count_tokens(text).min(i32::MAX as u64) as i32
}

fn content_block_tokens(block: &Value) -> i32 {
    let mut total = block
        .get("text")
        .and_then(Value::as_str)
        .map(count_text)
        .unwrap_or(0);
    if let Some(thinking) = block.get("thinking").and_then(Value::as_str) {
        total = total.saturating_add(count_text(thinking));
    }
    if block.get("type").and_then(Value::as_str) == Some("tool_use")
        && let Some(input) = block.get("input")
    {
        total = total.saturating_add(count_text(&input.to_string()));
    }
    if block.get("type").and_then(Value::as_str) == Some("tool_result")
        && let Some(content) = block.get("content")
    {
        total = total.saturating_add(count_text(&content.to_string()));
    }
    total
}

fn ttl(control: &CacheControl) -> Duration {
    let Some(ttl) = control.ttl.as_deref() else {
        return DEFAULT_TTL;
    };
    let (number, multiplier) = if let Some(value) = ttl.strip_suffix('s') {
        (value, 1)
    } else if let Some(value) = ttl.strip_suffix('m') {
        (value, 60)
    } else if let Some(value) = ttl.strip_suffix('h') {
        (value, 60 * 60)
    } else if let Some(value) = ttl.strip_suffix('d') {
        (value, 24 * 60 * 60)
    } else {
        return DEFAULT_TTL;
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|seconds| seconds.checked_mul(multiplier))
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TTL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::MessagesRequest;

    fn request(ttl: Option<&str>) -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 32,
            "system": [{
                "type": "text",
                "text": "stable instructions",
                "cache_control": {
                    "type": "ephemeral",
                    "ttl": ttl
                }
            }],
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap()
    }

    #[test]
    fn first_request_creates_and_second_reads_cache() {
        let req = request(Some("5m"));
        let first = record_request(&req);
        let second = record_request(&req);

        assert!(first.cache_creation_input_tokens > 0);
        assert_eq!(
            first.cache_creation_5m_input_tokens,
            first.cache_creation_input_tokens
        );
        assert_eq!(first.cache_creation_1h_input_tokens, 0);
        assert_eq!(first.cache_read_input_tokens, 0);
        assert_eq!(second.cache_creation_input_tokens, 0);
        assert_eq!(
            second.cache_read_input_tokens,
            first.cache_creation_input_tokens
        );
    }

    #[test]
    fn request_without_breakpoint_has_no_cache_usage() {
        let mut req = request(None);
        req.system.as_mut().unwrap()[0].cache_control = None;
        assert_eq!(record_request(&req), CacheUsage::default());
    }

    #[test]
    fn changing_content_after_breakpoint_keeps_cache_hit() {
        let mut first_request = request(Some("5m"));
        first_request.system.as_mut().unwrap()[0].text =
            "stable instructions for tail-change test".to_string();
        let first = record_request(&first_request);

        let mut second_request = first_request;
        second_request.messages[0].content = json!("different dynamic user input");
        let second = record_request(&second_request);

        assert!(first.cache_creation_input_tokens > 0);
        assert_eq!(
            second.cache_read_input_tokens,
            first.cache_creation_input_tokens
        );
    }

    #[test]
    fn bounded_usage_keeps_cached_and_uncached_tokens_separate() {
        let usage = CacheUsage {
            cache_creation_input_tokens: 80,
            cache_read_input_tokens: 0,
            cache_creation_5m_input_tokens: 80,
            ..CacheUsage::default()
        }
        .bounded(100);
        assert_eq!(usage.uncached_input_tokens(100), 20);
        assert_eq!(usage.cache_creation_input_tokens, 80);

        let mixed = CacheUsage {
            cache_creation_input_tokens: 40,
            cache_read_input_tokens: 50,
            cache_creation_5m_input_tokens: 40,
            ..CacheUsage::default()
        }
        .bounded(100);
        assert_eq!(mixed.uncached_input_tokens(100), 10);
    }

    #[test]
    fn falls_back_to_longest_stable_breakpoint() {
        let first_request: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 32,
            "system": [{
                "type": "text",
                "text": "unique stable multi-breakpoint system",
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "first extended prefix",
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }))
        .unwrap();
        let first = record_request(&first_request);

        let mut second_request = first_request;
        second_request.messages[0].content[0]["text"] = json!("changed extended prefix");
        let second = record_request(&second_request);

        assert!(first.cache_creation_input_tokens > 0);
        assert!(second.cache_read_input_tokens > 0);
        assert!(second.cache_creation_input_tokens > 0);
        assert!(
            second.cache_read_input_tokens + second.cache_creation_input_tokens
                >= first.cache_creation_input_tokens
        );
    }

    #[test]
    fn expired_entry_is_created_again() {
        let mut req = request(Some("0s"));
        req.system.as_mut().unwrap()[0].text = "unique immediately expired prefix".to_string();

        let first = record_request(&req);
        let second = record_request(&req);

        assert!(first.cache_creation_input_tokens > 0);
        assert!(second.cache_creation_input_tokens > 0);
        assert_eq!(second.cache_read_input_tokens, 0);
    }

    #[test]
    fn one_hour_ttl_is_reported_separately() {
        let mut req = request(Some("1h"));
        req.system.as_mut().unwrap()[0].text = "unique one-hour cache prefix".to_string();

        let usage = record_request(&req);

        assert!(usage.cache_creation_input_tokens > 0);
        assert_eq!(usage.cache_creation_5m_input_tokens, 0);
        assert_eq!(
            usage.cache_creation_1h_input_tokens,
            usage.cache_creation_input_tokens
        );
    }
}
