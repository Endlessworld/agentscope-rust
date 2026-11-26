//! `providers/set` over ACP v2 must persist what the client puts in `_meta`.
//!
//! The standard v1 request shape has no model-table or enabled flag; this agent carries both
//! in `_meta` (`models`, `disabled`, `label`). The v2 graph translates its request into the v1
//! shape before calling the shared `providers_admin` implementation, so the translation has to
//! forward `_meta` too. Regression test for that drop: enabling a disabled provider and
//! updating its model table used to answer `{}` while leaving `models.json` untouched.
#![cfg(feature = "acp-v2")]

use std::path::PathBuf;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v2::schema as v2;
use agent_client_protocol::v2::ConnectionTo;
use agent_client_protocol::{Agent, Channel, Client};
use agentscope_extensions_acp::AcpCodingAgent;

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agentscope_acp_v2_providers_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[tokio::test]
async fn v2_provider_set_persists_models_and_enabled_flag() {
    let root = temp_root();
    let config_path = root.join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_provider": "Gateway",
            "default_model": "gpt-1",
            "providers": {
                "Gateway": {
                    "base_url": "https://gateway.example/v1",
                    "api_key": "gateway-key",
                    "models": { "completions": ["gpt-1"] },
                    "disabled": true
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    // Sole test in this binary: no other test reads or writes the same config path.
    std::env::set_var("AGENTSCOPE_MODELS_JSON", &config_path);

    let (agent_channel, client_channel) = Channel::duplex();
    let agent = AcpCodingAgent::builder()
        .manage_providers(true)
        .build()
        .expect("build AcpCodingAgent");
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let listed = Client
        .v2()
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("v2-providers-test", "0.0.0"),
                    ))
                    .block_task()
                    .await?;

                // The client's "enable + save model list" edit: an empty base URL means
                // "keep the saved one", everything else rides in `_meta`.
                let mut meta = serde_json::Map::new();
                meta.insert("disabled".to_string(), serde_json::json!(false));
                meta.insert("label".to_string(), serde_json::json!("Gateway (UI)"));
                meta.insert(
                    "models".to_string(),
                    serde_json::json!({ "completions": ["gpt-1", "gpt-2"] }),
                );
                connection
                    .send_request(
                        v2::SetProviderRequest::new("Gateway", v2::LlmProtocol::OpenAi, "")
                            .meta(meta),
                    )
                    .block_task()
                    .await?;

                connection
                    .send_request(v2::ListProvidersRequest::new())
                    .block_task()
                    .await
            },
        )
        .await
        .expect("v2 client run");

    let gateway = listed
        .providers
        .iter()
        .find(|provider| provider.provider_id.to_string() == "Gateway")
        .expect("Gateway is still listed");
    let meta = gateway.meta.as_ref().expect("provider metadata");
    assert_eq!(meta.get("disabled"), Some(&serde_json::json!(false)));
    assert_eq!(meta.get("label"), Some(&serde_json::json!("Gateway (UI)")));
    assert_eq!(
        meta.get("models"),
        Some(&serde_json::json!({ "completions": ["gpt-1", "gpt-2"] }))
    );
    assert!(
        gateway.current.is_some(),
        "an enabled provider advertises its current routing config"
    );

    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).expect("models.json written"))
            .expect("valid json");
    let saved = &written["providers"]["Gateway"];
    assert_eq!(
        saved["models"],
        serde_json::json!({ "completions": ["gpt-1", "gpt-2"] }),
        "the model table must land on disk"
    );
    assert_eq!(
        saved["label"],
        serde_json::json!("Gateway (UI)"),
        "the display label must land on disk"
    );
    assert!(
        saved.get("disabled").is_none(),
        "disabled=false is the default and is omitted, got {saved}"
    );

    agent_task.abort();
    std::env::remove_var("AGENTSCOPE_MODELS_JSON");
    let _ = std::fs::remove_dir_all(&root);
}
