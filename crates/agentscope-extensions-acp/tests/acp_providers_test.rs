//! The managed `providers/*` surface: the agent treats `models.json` as the single source of
//! truth, and a provider edit reaches a **live but idle** session (`session/new` binds the
//! connection, so `config_option_update` can be pushed before the first prompt).
//!
//! The config path is redirected with `AGENTSCOPE_MODELS_JSON`, so the developer's real
//! `~/.agentscope/models.json` is never read or written. Everything runs in-process over
//! `Channel::duplex` with a scripted model.
mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    InitializeRequest, ListProvidersRequest, LlmProtocol, NewSessionRequest, SessionConfigKind,
    SessionConfigSelectOptions, SessionNotification, SessionUpdate, SetProviderRequest,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{Agent, Channel, Client};
use agentscope_extensions_acp::config::ModelChoice;
use agentscope_extensions_acp::AcpCodingAgent;
use agentscope_harness::agent::HarnessAgent;

fn fresh_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("agentscope_acp_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// The model option of a `config_option_update`, or `None` for any other notification.
fn model_option(notification: &SessionNotification) -> Option<(String, Vec<String>)> {
    let SessionUpdate::ConfigOptionUpdate(update) = &notification.update else {
        return None;
    };
    let option = update
        .config_options
        .iter()
        .find(|option| option.id.to_string() == "model")?;
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    let ids = match &select.options {
        SessionConfigSelectOptions::Ungrouped(list) => {
            list.iter().map(|option| option.value.to_string()).collect()
        }
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().map(|option| option.value.to_string()))
            .collect(),
        _ => return None,
    };
    Some((select.current_value.to_string(), ids))
}

async fn wait_for_model_option(
    seen: &Arc<Mutex<Vec<SessionNotification>>>,
    wanted: &str,
) -> (String, Vec<String>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        {
            let guard = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            for notification in guard.iter() {
                if let Some((current, ids)) = model_option(notification) {
                    if ids.iter().any(|id| id == wanted) {
                        return (current, ids);
                    }
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a config_option_update carrying '{wanted}'"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_provider_edit_reaches_an_idle_session_and_lands_on_disk() {
    let root = fresh_root("providers");
    let config_path = root.join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_provider": "Local",
            "default_model": "stub",
            "providers": {
                "Local": {
                    "base_url": "https://local.example/v1",
                    "api_key": "local-key",
                    "models": { "completions": ["stub"] }
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    // Sole-owner of this process: the integration test binary has no other test reading it.
    std::env::set_var("AGENTSCOPE_MODELS_JSON", &config_path);

    let model = ModelChoice {
        id: "stub".to_string(),
        name: "Local/stub".to_string(),
        provider: Some("Local".to_string()),
        catalog_key: None,
        base_url: Some("https://local.example/v1".to_string()),
    };
    let factory_root = root.clone();
    let agent = AcpCodingAgent::builder()
        .models(vec![model])
        .manage_providers(true)
        .model_factory(move |cwd: PathBuf, session_id: &str| {
            HarnessAgent::builder()
                .name("acp-providers-test")
                .model(Arc::new(common::StubModel::new("done")))
                .session_id(session_id)
                .cwd(cwd)
                .workspace(factory_root.join(".agentscope").join("workspace"))
                .disable_memory_hooks()
                .max_iters(1)
                .build()
        })
        .build()
        .expect("build AcpCodingAgent");

    let (agent_channel, client_channel) = Channel::duplex();
    let agent_task = tokio::spawn(async move { agent.run_with_transport(agent_channel).await });

    let seen: Arc<Mutex<Vec<SessionNotification>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_notifications = Arc::clone(&seen);
    let root_for_client = root.clone();
    let result = Client
        .v1()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                seen_notifications
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            client_channel,
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                connection
                    .send_request(NewSessionRequest::new(root_for_client))
                    .block_task()
                    .await?;

                // No prompt: the session is idle, yet it must still hear about the change.
                let mut meta = serde_json::Map::new();
                meta.insert(
                    "models".to_string(),
                    serde_json::json!({ "completions": ["extra-1"] }),
                );
                connection
                    .send_request(
                        SetProviderRequest::new(
                            "Extra",
                            LlmProtocol::OpenAi,
                            "https://extra.example/v1",
                        )
                        .headers(
                            [("Authorization".to_string(), "Bearer extra-key".to_string())]
                                .into_iter()
                                .collect(),
                        )
                        .meta(meta),
                    )
                    .block_task()
                    .await?;

                let providers = connection
                    .send_request(ListProvidersRequest::new())
                    .block_task()
                    .await?;
                Ok::<_, agent_client_protocol::Error>(providers)
            },
        )
        .await;

    let (current, ids) = wait_for_model_option(&seen, "extra-1").await;
    assert_eq!(
        current, "stub",
        "the selected model must survive a provider addition"
    );
    assert!(
        ids.iter().any(|id| id == "stub"),
        "the old model stays: {ids:?}"
    );

    let providers = result.expect("set + list round trip").providers;
    let extra = providers
        .iter()
        .find(|provider| provider.provider_id.to_string() == "Extra")
        .expect("the new provider is listed");
    assert_eq!(
        extra
            .meta
            .as_ref()
            .and_then(|meta| meta.get("models"))
            .cloned(),
        Some(serde_json::json!({ "completions": ["extra-1"] }))
    );
    // The key never travels back to the client.
    assert_eq!(
        extra
            .meta
            .as_ref()
            .and_then(|meta| meta.get("apiKeyMasked"))
            .and_then(|value| value.as_str())
            .map(|masked| masked.contains("extra-key")),
        Some(false)
    );

    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).expect("models.json written"))
            .expect("valid json");
    assert_eq!(
        written["providers"]["Extra"]["models"],
        serde_json::json!({ "completions": ["extra-1"] })
    );
    assert!(
        written["providers"].get("Local").is_some(),
        "an edit must keep the providers it did not touch"
    );

    agent_task.abort();
    std::env::remove_var("AGENTSCOPE_MODELS_JSON");
    let _ = std::fs::remove_dir_all(&root);
}
