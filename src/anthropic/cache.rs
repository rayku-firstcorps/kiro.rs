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

    /// Attribute all but one input token to cache accounting when this request
    /// contains an active cache breakpoint. This is only a billing compatibility
    /// view; Kiro still receives and processes the complete request.
    pub fn high_cache(self, total_input_tokens: i32) -> Self {
        let total = total_input_tokens.max(0);
        let cached_total = total.saturating_sub(1);
        let mut usage = self.bounded(cached_total);
        let remainder = usage.uncached_input_tokens(cached_total);
        if remainder == 0 {
            return usage;
        }

        if usage.cache_read_input_tokens > 0 {
            usage.cache_read_input_tokens = usage.cache_read_input_tokens.saturating_add(remainder);
        } else if usage.cache_creation_input_tokens > 0 {
            usage.cache_creation_input_tokens =
                usage.cache_creation_input_tokens.saturating_add(remainder);
            if usage.cache_creation_1h_input_tokens > 0 && usage.cache_creation_5m_input_tokens == 0
            {
                usage.cache_creation_1h_input_tokens = usage
                    .cache_creation_1h_input_tokens
                    .saturating_add(remainder);
            } else {
                usage.cache_creation_5m_input_tokens = usage
                    .cache_creation_5m_input_tokens
                    .saturating_add(remainder);
            }
        }

        usage
    }
}

#[derive(Debug)]
struct CacheEntry {
    expires_at: Instant,
    input_tokens: i32,
    ttl: Duration,
}

#[derive(Debug)]
struct Prefix {
    key: String,
    input_tokens: i32,
}

#[derive(Debug)]
struct CacheRequest {
    prefixes: Vec<Prefix>,
    breakpoints: Vec<(usize, Duration)>,
}

static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();

/// Record Anthropic cache breakpoints in a request.
pub fn record_request(req: &MessagesRequest) -> CacheUsage {
    let cache_request = cache_request(req);
    if cache_request.breakpoints.is_empty() {
        return CacheUsage::default();
    }

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    let mut entries = cache.lock().expect("prompt cache lock poisoned");
    entries.retain(|_, entry| entry.expires_at > now);

    // A cache marker commonly moves to the newest message on every turn. Search
    // every earlier boundary beneath the current markers so a previously cached
    // shorter prefix can still be reused.
    let matched_key = cache_request
        .breakpoints
        .iter()
        .filter_map(|(breakpoint_index, _)| {
            cache_request.prefixes[..=*breakpoint_index]
                .iter()
                .rev()
                .find(|prefix| entries.contains_key(&prefix.key))
        })
        .max_by_key(|prefix| prefix.input_tokens)
        .map(|prefix| prefix.key.clone());
    let cache_read_input_tokens = matched_key
        .as_ref()
        .and_then(|key| entries.get(key))
        .map(|entry| entry.input_tokens)
        .unwrap_or(0);
    if let Some(key) = matched_key
        && let Some(entry) = entries.get_mut(&key)
    {
        entry.expires_at = now + entry.ttl;
    }

    let mut accounted_input_tokens = cache_read_input_tokens;
    let mut cache_creation_5m_input_tokens = 0_i32;
    let mut cache_creation_1h_input_tokens = 0_i32;

    for (breakpoint_index, ttl) in cache_request.breakpoints {
        let prefix = &cache_request.prefixes[breakpoint_index];
        if !entries.contains_key(&prefix.key) && prefix.input_tokens > accounted_input_tokens {
            let created = prefix.input_tokens.saturating_sub(accounted_input_tokens);
            if ttl >= Duration::from_secs(60 * 60) {
                cache_creation_1h_input_tokens =
                    cache_creation_1h_input_tokens.saturating_add(created);
            } else {
                cache_creation_5m_input_tokens =
                    cache_creation_5m_input_tokens.saturating_add(created);
            }
            accounted_input_tokens = prefix.input_tokens;
        }
        entries
            .entry(prefix.key.clone())
            .and_modify(|entry| entry.expires_at = now + entry.ttl)
            .or_insert(CacheEntry {
                expires_at: now + ttl,
                input_tokens: prefix.input_tokens,
                ttl,
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

/// Build deterministic hashes for every request boundary and record which of
/// those boundaries contain Anthropic cache markers.
fn cache_request(req: &MessagesRequest) -> CacheRequest {
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
            let tool_value = serde_json::to_value(tool).unwrap_or(Value::Null);
            segments.push(json!({
                "role": "tool",
                "tool": content_without_cache_control(tool_value)
            }));
            let schema = serde_json::to_string(&canonicalize_json(
                serde_json::to_value(&tool.input_schema).unwrap_or(Value::Null),
            ))
            .unwrap_or_default();
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
                    segments.push(json!({
                        "role": message.role,
                        "content": content_without_cache_control(block.clone())
                    }));
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

    let mut hasher = Sha256::new();
    hasher.update((req.model.len() as u64).to_be_bytes());
    hasher.update(req.model.as_bytes());
    let mut input_tokens = 0_i32;
    let prefixes = segments
        .into_iter()
        .zip(token_counts)
        .map(|(segment, segment_tokens)| {
            let segment = canonicalize_json(segment);
            let bytes = serde_json::to_vec(&segment).unwrap_or_default();
            hasher.update((bytes.len() as u64).to_be_bytes());
            hasher.update(bytes);
            input_tokens = input_tokens.saturating_add(segment_tokens);
            Prefix {
                key: format!("{:x}", hasher.clone().finalize()),
                input_tokens: input_tokens.max(1),
            }
        })
        .collect();

    CacheRequest {
        prefixes,
        breakpoints,
    }
}

fn content_without_cache_control(mut value: Value) -> Value {
    if let Value::Object(fields) = &mut value {
        fields.remove("cache_control");
    }
    canonicalize_json(value)
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        Value::Object(fields) => {
            let mut fields: Vec<_> = fields.into_iter().collect();
            fields.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| (key, canonicalize_json(value)))
                    .collect(),
            )
        }
        scalar => scalar,
    }
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
    fn moving_breakpoint_to_new_message_reuses_previous_prefix() {
        let first_request: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 32,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "unique first turn for moving breakpoint",
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }))
        .unwrap();
        let first = record_request(&first_request);

        let second_request: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 32,
            "messages": [
                {
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": "unique first turn for moving breakpoint"
                    }]
                },
                {"role": "assistant", "content": "first response"},
                {
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": "unique second turn for moving breakpoint",
                        "cache_control": {"type": "ephemeral"}
                    }]
                }
            ]
        }))
        .unwrap();
        let second = record_request(&second_request);

        assert!(first.cache_creation_input_tokens > 0);
        assert_eq!(
            second.cache_read_input_tokens,
            first.cache_creation_input_tokens
        );
        assert!(second.cache_creation_input_tokens > 0);

        let billed = second.high_cache(100);
        assert_eq!(billed.uncached_input_tokens(100), 1);
        assert_eq!(
            billed.cache_read_input_tokens + billed.cache_creation_input_tokens,
            99
        );
        assert!(billed.cache_read_input_tokens > billed.cache_creation_input_tokens);
    }

    #[test]
    fn cache_control_metadata_does_not_change_prefix_key() {
        let with_control: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 32,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "same semantic cache content",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }]
            }]
        }))
        .unwrap();
        let without_control: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 32,
            "messages": [{
                "role": "user",
                "content": [{
                    "text": "same semantic cache content",
                    "type": "text"
                }]
            }]
        }))
        .unwrap();

        assert_eq!(
            cache_request(&with_control).prefixes[0].key,
            cache_request(&without_control).prefixes[0].key
        );
    }

    #[test]
    fn canonical_tool_schema_has_stable_prefix_key() {
        let first: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 32,
            "tools": [{
                "name": "unique_schema_tool",
                "description": "schema ordering test",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "alpha": {"type": "string", "description": "first"},
                        "beta": {"description": "second", "type": "number"}
                    }
                },
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        let second: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 32,
            "tools": [{
                "description": "schema ordering test",
                "name": "unique_schema_tool",
                "input_schema": {
                    "properties": {
                        "beta": {"type": "number", "description": "second"},
                        "alpha": {"description": "first", "type": "string"}
                    },
                    "type": "object"
                },
                "cache_control": {"ttl": "5m", "type": "ephemeral"}
            }],
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();

        assert_eq!(
            cache_request(&first).prefixes[0].key,
            cache_request(&second).prefixes[0].key
        );
    }

    #[test]
    fn cache_hit_refreshes_ttl() {
        let mut req = request(Some("5m"));
        req.system.as_mut().unwrap()[0].text = "unique ttl refresh prefix".to_string();
        record_request(&req);
        let key = cache_request(&req).prefixes[0].key.clone();
        let cache = CACHE.get().unwrap();
        {
            let mut entries = cache.lock().unwrap();
            entries.get_mut(&key).unwrap().expires_at = Instant::now() + Duration::from_secs(1);
        }

        let usage = record_request(&req);
        let entries = cache.lock().unwrap();
        let remaining = entries[&key]
            .expires_at
            .saturating_duration_since(Instant::now());

        assert!(usage.cache_read_input_tokens > 0);
        assert!(remaining > Duration::from_secs(4 * 60));
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
    fn high_cache_keeps_one_input_token_and_caches_the_rest() {
        let created = CacheUsage {
            cache_creation_input_tokens: 80,
            cache_creation_5m_input_tokens: 80,
            ..CacheUsage::default()
        }
        .high_cache(100);
        assert_eq!(created.cache_creation_input_tokens, 99);
        assert_eq!(created.cache_creation_5m_input_tokens, 99);
        assert_eq!(created.uncached_input_tokens(100), 1);

        let read = CacheUsage {
            cache_read_input_tokens: 80,
            ..CacheUsage::default()
        }
        .high_cache(100);
        assert_eq!(read.cache_read_input_tokens, 99);
        assert_eq!(read.uncached_input_tokens(100), 1);

        let uncached = CacheUsage::default().high_cache(100);
        assert_eq!(uncached, CacheUsage::default());
        assert_eq!(uncached.uncached_input_tokens(100), 100);
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
