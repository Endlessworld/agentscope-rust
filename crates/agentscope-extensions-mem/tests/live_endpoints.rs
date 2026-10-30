//! Environment-gated live endpoint checks (tracker §3.2 「真实端点联调」).
//!
//! These tests are **not** `#[ignore]`d: they run normally and return immediately
//! (passing) when the vendor credentials are absent, so CI stays hermetic. Set the
//! documented environment variables to exercise the real services:
//!
//! | vendor | variables |
//! |---|---|
//! | Mem0 | `MEM0_LIVE_BASE_URL` (required), `MEM0_LIVE_API_KEY` (optional) |
//! | Bailian | `BAILIAN_LIVE_API_KEY` (required), `BAILIAN_LIVE_BASE_URL` (optional) |
//! | ReMe | `REME_LIVE_BASE_URL` (required) |
//!
//! Each test performs one `record` + one `retrieve` round trip with a unique user id
//! and only asserts that the call succeeds — memory extraction on the vendor side is
//! asynchronous, so asserting on retrieved content would be flaky.

use std::env;
use std::time::Duration;

use agentscope_core::{Msg, MsgRole};
use agentscope_extensions_mem::{
    BailianLongTermMemory, BailianLongTermMemoryConfig, BailianMemoryClient, Mem0Client,
    Mem0LongTermMemory, Mem0LongTermMemoryConfig, ReMeClient, ReMeLongTermMemory,
};

fn non_blank_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn unique_user_id(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{prefix}-{nanos}")
}

fn probe_message() -> Msg {
    Msg::builder()
        .role(MsgRole::User)
        .text_content("The integration probe user prefers jasmine tea.")
        .build()
}

#[tokio::test]
async fn mem0_live_round_trip_when_configured() {
    let Some(base_url) = non_blank_env("MEM0_LIVE_BASE_URL") else {
        return;
    };
    let client = Mem0Client::new(base_url, non_blank_env("MEM0_LIVE_API_KEY"))
        .expect("mem0 client should build");
    let adapter = Mem0LongTermMemory::new(
        std::sync::Arc::new(client),
        Mem0LongTermMemoryConfig {
            user_id: Some(unique_user_id("agentscope-mem-live")),
            ..Default::default()
        },
    )
    .expect("mem0 adapter should build");
    adapter.record(&[probe_message()]).await.expect("mem0 record");
    // Memory extraction is asynchronous on the Mem0 side, so only reachability of
    // the search endpoint is asserted here.
    adapter
        .retrieve(&probe_message())
        .await
        .expect("mem0 retrieve");
}

#[tokio::test]
async fn bailian_live_round_trip_when_configured() {
    let Some(api_key) = non_blank_env("BAILIAN_LIVE_API_KEY") else {
        return;
    };
    let client = BailianMemoryClient::with_timeout(
        api_key.clone(),
        non_blank_env("BAILIAN_LIVE_BASE_URL"),
        Duration::from_secs(60),
    )
    .expect("bailian client should build");
    let config = BailianLongTermMemoryConfig::new(api_key, unique_user_id("agentscope-mem-live"));
    let adapter = BailianLongTermMemory::new(std::sync::Arc::new(client), config)
        .expect("bailian adapter should build");
    adapter
        .record(&[probe_message()])
        .await
        .expect("bailian record");
    adapter
        .retrieve(&probe_message())
        .await
        .expect("bailian retrieve");
}

#[tokio::test]
async fn reme_live_round_trip_when_configured() {
    let Some(base_url) = non_blank_env("REME_LIVE_BASE_URL") else {
        return;
    };
    let client = ReMeClient::with_timeout(base_url, Duration::from_secs(60))
        .expect("reme client should build");
    let adapter = ReMeLongTermMemory::new(
        std::sync::Arc::new(client),
        unique_user_id("agentscope-mem-live"),
    )
    .expect("reme adapter should build");
    adapter.record(&[probe_message()]).await.expect("reme record");
    adapter
        .retrieve(&probe_message())
        .await
        .expect("reme retrieve");
}
