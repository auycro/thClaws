//! Live provider auth integration tests — every test in this file
//! makes a REAL HTTP request to a remote LLM provider, so each one is
//! `#[ignore]` and skipped on a normal `cargo test`. Run explicitly:
//!
//!     cargo test --features gui --test live_provider_auth -- --ignored
//!
//! Or one at a time:
//!
//!     cargo test --features gui --test live_provider_auth openrouter -- --ignored
//!
//! ## What these cover that the unit tests don't
//!
//! Issue #145 (OpenRouter "Missing Authentication header" — only free
//! models in the reporter's usage) was a wrapped-quote key landing
//! verbatim in the keychain → `Authorization: Bearer "sk-…"` on the
//! wire. Every unit-test code path that exercised auth used a
//! synthetic key, so the mismatch between "what the user typed" and
//! "what the wire saw" was invisible. These tests close that gap by
//! ALSO sending the request to the real provider, so a key-shape
//! regression surfaces as a 401 rather than a green unit test.
//!
//! ## Cost & skip behaviour
//!
//! Each test sends `max_tokens: 12`, a single 4-char prompt, and
//! reads only the first chunk before dropping the stream. Cost per
//! run is well under $0.001 across all providers combined. Tests skip
//! cleanly (no fail, no panic) when their provider's env var is
//! unset, so the file is safe to bulk-ignore-run on a workstation
//! that only has some keys configured.
//!
//! ## What to add when…
//!
//! - A new provider lands in `ProviderKind::ALL`: add a sibling
//!   `live_<name>_…` test mirroring `live_openrouter_free_model_works`.
//! - A new auth-handling code path lands (gateway overlay flag,
//!   per-model auth quirk, etc.): add a focused live test that
//!   exercises ONLY that path so the failure mode is unambiguous.

use std::time::Duration;

use futures::StreamExt;
use thclaws_core::providers::{Provider, StreamRequest};
use thclaws_core::types::{ContentBlock, Message, Role};

/// One-shot prompt + tiny budget. Keeps live-test cost negligible.
fn tiny_request(model: &str) -> StreamRequest {
    StreamRequest {
        model: model.to_string(),
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::text("hi")],
        }],
        tools: vec![],
        max_tokens: 12,
        thinking_budget: None,
        // Live tests should fail fast, not hang for the default 120s
        // when the network or provider misbehaves.
        stream_chunk_timeout_override: Some(Duration::from_secs(20)),
    }
}

/// Read up to the first 3 chunks of a stream, return Err on a
/// provider-level error. Three chunks is enough to confirm the
/// handshake completed (auth ok, headers ok, JSON ok) without
/// running the full response.
async fn drain_a_few(mut stream: thclaws_core::providers::EventStream) -> Result<(), String> {
    for _ in 0..3 {
        match tokio::time::timeout(Duration::from_secs(20), stream.next()).await {
            Ok(Some(Ok(_ev))) => { /* successful chunk */ }
            Ok(Some(Err(e))) => return Err(format!("{e}")),
            Ok(None) => break,
            Err(_) => return Err("stream idle 20s — provider stopped sending".into()),
        }
    }
    Ok(())
}

// ─── OpenRouter ──────────────────────────────────────────────────

/// Issue #145 regression: a free model on OpenRouter must auth and
/// respond. Reporter saw `Missing Authentication header` for
/// `openai/gpt-oss-120b:free` specifically; we now know the auth
/// path doesn't branch on `:free`, but a live test guards against
/// any future routing change that would.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn live_openrouter_free_model_works() {
    let Some(key) = std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        eprintln!("SKIP: OPENROUTER_API_KEY not set");
        return;
    };
    let provider = thclaws_core::providers::openai::OpenAIProvider::new(key)
        .with_base_url("https://openrouter.ai/api/v1/chat/completions".to_string())
        .with_strip_model_prefix("openrouter/".to_string());
    let stream = provider
        .stream(tiny_request("openrouter/openai/gpt-oss-120b:free"))
        .await
        .expect("stream open");
    if let Err(e) = drain_a_few(stream).await {
        panic!("openrouter free-model stream failed: {e}");
    }
}

/// Paid model on the same provider — control for the free-model
/// test. If both fail, the issue is auth; if only one fails, the
/// issue is model-specific routing.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn live_openrouter_paid_model_works() {
    let Some(key) = std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        eprintln!("SKIP: OPENROUTER_API_KEY not set");
        return;
    };
    let provider = thclaws_core::providers::openai::OpenAIProvider::new(key)
        .with_base_url("https://openrouter.ai/api/v1/chat/completions".to_string())
        .with_strip_model_prefix("openrouter/".to_string());
    let stream = provider
        .stream(tiny_request("openrouter/anthropic/claude-3.5-haiku"))
        .await
        .expect("stream open");
    if let Err(e) = drain_a_few(stream).await {
        panic!("openrouter paid-model stream failed: {e}");
    }
}

/// Direct repro of issue #145: wrap the openrouter key in literal
/// double quotes (the copy-paste-from-.env shape) and verify the
/// request fails with the EXACT message the reporter saw —
/// `Missing Authentication header`. If a future change to
/// `OpenAIProvider::auth_header_value` happens to sanitise quotes
/// in-flight, this test will fail loudly and we know to revisit the
/// scope of the fix at `api_key_set` / `api_key_from_env`.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn live_openrouter_wrapped_quote_key_reproduces_issue_145() {
    let Some(key) = std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        eprintln!("SKIP: OPENROUTER_API_KEY not set");
        return;
    };
    let wrapped = format!("\"{key}\"");
    let provider = thclaws_core::providers::openai::OpenAIProvider::new(wrapped)
        .with_base_url("https://openrouter.ai/api/v1/chat/completions".to_string())
        .with_strip_model_prefix("openrouter/".to_string());
    let result = provider
        .stream(tiny_request("openrouter/openai/gpt-oss-120b:free"))
        .await;
    let err = match result {
        Err(e) => format!("{e}"),
        Ok(stream) => drain_a_few(stream).await.unwrap_err(),
    };
    assert!(
        err.contains("Missing Authentication header"),
        "wrapped-quote key should reproduce #145 — got: {err}"
    );
}

// ─── Anthropic ───────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn live_anthropic_works() {
    let Some(key) = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        eprintln!("SKIP: ANTHROPIC_API_KEY not set");
        return;
    };
    let provider = thclaws_core::providers::anthropic::AnthropicProvider::new(key);
    let stream = provider
        .stream(tiny_request("claude-haiku-4-5"))
        .await
        .expect("stream open");
    if let Err(e) = drain_a_few(stream).await {
        panic!("anthropic stream failed: {e}");
    }
}

// ─── OpenAI ──────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn live_openai_works() {
    let Some(key) = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        eprintln!("SKIP: OPENAI_API_KEY not set");
        return;
    };
    let provider = thclaws_core::providers::openai::OpenAIProvider::new(key);
    let stream = provider
        .stream(tiny_request("gpt-4o-mini"))
        .await
        .expect("stream open");
    if let Err(e) = drain_a_few(stream).await {
        panic!("openai stream failed: {e}");
    }
}

// ─── Gemini ──────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn live_gemini_works() {
    let Some(key) = std::env::var("GOOGLE_API_KEY")
        .ok()
        .or_else(|| std::env::var("GEMINI_API_KEY").ok())
        .filter(|s| !s.is_empty())
    else {
        eprintln!("SKIP: GOOGLE_API_KEY / GEMINI_API_KEY not set");
        return;
    };
    let provider = thclaws_core::providers::gemini::GeminiProvider::new(key);
    let stream = provider
        .stream(tiny_request("gemini-2.5-flash"))
        .await
        .expect("stream open");
    if let Err(e) = drain_a_few(stream).await {
        panic!("gemini stream failed: {e}");
    }
}
