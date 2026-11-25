//! 供应商管理的 ACP 表面。
//!
//! 两个面：
//!
//! - 标准面 `providers/list|set|disable`（ACP `unstable_llm_providers`）：
//!   `list` 把 `~/.agentscope/models.json` 里的每个 provider 一条 `ProviderInfo` 送出去，
//!   `set` 新增/改写一条，`disable` 只打停用标记（密钥保留，可再启用）。
//! - 扩展面 `_acp_extends/providers/*`：标准面装不下的三件事——
//!   `remove`（删除条目）、`default`（改默认 provider/model）、`catalog`（models.dev
//!   预设，选厂商即预填 base_url + 协议 + 模型表）、`probe`（用保存的密钥去
//!   `GET {base_url}/models`，既是“测试连接”也是“拉取模型列表”。
//!
//! 密钥不上线：`list` 只给掩码（只留首尾几位），`set` 从 `Authorization: Bearer …`
//! （或 `x-api-key`）里取。标准面没有模型表的字段，因此模型表走 `_meta.models`，
//! 形状与配置文件里的 `models` 完全一致（分组对象或扁平数组）。

use std::sync::Arc;

use agent_client_protocol::v1::schema::{
    LlmProtocol, ProviderCurrentConfig, ProviderInfo, SetProviderRequest,
};
use agent_client_protocol::{JsonRpcMessage, JsonRpcRequest, UntypedMessage};
use agentscope_core::model::provider::ApiProtocol;
use agentscope_extensions_model::common::{HttpTransport, ReqwestTransport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::config::ModelChoice;
use crate::model_config::{mask_secret, ModelConfig, ModelsSpec, ProviderUpsert};

/// 删除一个 provider 条目。
pub const REMOVE_METHOD: &str = "_acp_extends/providers/remove";
/// 改默认 provider / 模型。
pub const DEFAULT_METHOD: &str = "_acp_extends/providers/default";
/// models.dev 预设列表（可带 `providerId` 取单个预设的模型表）。
pub const CATALOG_METHOD: &str = "_acp_extends/providers/catalog";
/// 用保存的凭据访问 `GET {base_url}/models`，充当“测试连接”与“拉取模型”。
pub const PROBE_METHOD: &str = "_acp_extends/providers/probe";

/// 扩展方法的入参（四个方法共用一个形状，未用到的字段省略）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtParams {
    /// provider 键名（`remove` / `default` / `probe` / `catalog` 用）。
    #[serde(default)]
    pub provider_id: Option<String>,
    /// 默认模型（`default` 用；缺省取该 provider 的第一个模型）。
    #[serde(default)]
    pub model: Option<String>,
    /// 未保存的候选端点（`probe` 用，优先于已存配置）。
    #[serde(default)]
    pub base_url: Option<String>,
    /// 未保存的候选密钥（`probe` 用，优先于已存配置）。
    #[serde(default)]
    pub api_key: Option<String>,
    /// 候选协议（`probe` 用：`messages` / `responses` / `completions`）。
    #[serde(default)]
    pub protocol: Option<String>,
}

/// 一条 `_acp_extends/providers/*` 请求。
#[derive(Debug, Clone)]
pub struct ProvidersExtRequest {
    method: Arc<str>,
    pub params: ExtParams,
}

/// Runs one `_acp_extends/providers/*` request.
///
/// Shared by the v1 and v2 graphs: the extension methods are protocol-agnostic, so both graphs
/// answer through this one implementation.
pub async fn run_extension(
    method: &str,
    params: ExtParams,
) -> Result<(serde_json::Value, Option<Vec<ModelChoice>>), agent_client_protocol::Error> {
    // Network probing is the only asynchronous step; everything else is synchronous file work
    // and is pushed onto the blocking pool.
    if method == PROBE_METHOD {
        return Ok((probe(params).await, None));
    }
    let method = method.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        let mut changed = false;
        let value = match method.as_str() {
            REMOVE_METHOD => {
                changed = true;
                params
                    .provider_id
                    .as_deref()
                    .ok_or_else(|| "providerId is required".to_string())
                    .and_then(remove)
                    .map(|()| serde_json::json!({}))
            }
            DEFAULT_METHOD => {
                changed = true;
                params
                    .provider_id
                    .as_deref()
                    .ok_or_else(|| "providerId is required".to_string())
                    .and_then(|id| set_default(id, params.model.as_deref()))
                    .map(|()| serde_json::json!({}))
            }
            CATALOG_METHOD => Ok(serde_json::json!({
                "providers": catalog(params.provider_id.as_deref())
            })),
            other => Err(format!("unhandled provider extension '{other}'")),
        }?;
        let models = changed.then(choices);
        Ok::<_, String>((value, models))
    })
    .await;
    match outcome {
        Ok(Ok(pair)) => Ok(pair),
        Ok(Err(error)) => Err(agent_client_protocol::Error::invalid_params().data(error)),
        Err(error) => Err(agent_client_protocol::Error::invalid_params().data(error.to_string())),
    }
}

impl ProvidersExtRequest {
    /// The wire method this request arrived on (the four extension methods share one type).
    pub fn method_name(&self) -> &str {
        &self.method
    }
}

impl JsonRpcMessage for ProvidersExtRequest {
    fn matches_method(method: &str) -> bool {
        matches!(
            method,
            REMOVE_METHOD | DEFAULT_METHOD | CATALOG_METHOD | PROBE_METHOD
        )
    }

    fn method(&self) -> &str {
        &self.method
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, agent_client_protocol::Error> {
        UntypedMessage::new(self.method(), &self.params)
    }

    fn parse_message(method: &str, params: &Value) -> Result<Self, agent_client_protocol::Error> {
        if !Self::matches_method(method) {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        let params: ExtParams = serde_json::from_value(params.clone()).map_err(|error| {
            agent_client_protocol::Error::invalid_params().data(error.to_string())
        })?;
        Ok(Self {
            method: Arc::from(method),
            params,
        })
    }
}

impl JsonRpcRequest for ProvidersExtRequest {
    type Response = Value;
}

/// The config every mutation starts from: file **plus** the providers the environment supplies.
///
/// Those live in memory only ([`ModelConfig::save`] drops them), so loading the file alone would
/// see their entries as missing and write an empty one — the user's key would then stop being
/// picked up on the next start, and `models.json` would carry a provider without models.
fn load() -> ModelConfig {
    ModelConfig::load_or_create_with_env()
}

/// 当前可选的模型列表（已跳过停用的 provider）。
pub fn choices() -> Vec<ModelChoice> {
    load().all_models()
}

/// 配置里的每个 provider 一条 `ProviderInfo`。
pub fn list() -> Vec<ProviderInfo> {
    let config = load();
    config
        .sorted_keys()
        .into_iter()
        .filter_map(|id| {
            let provider = config.providers.get(&id)?;
            Some(describe(&config, &id, provider))
        })
        .collect()
}

fn describe(
    config: &ModelConfig,
    id: &str,
    provider: &crate::model_config::ModelProviderConfig,
) -> ProviderInfo {
    let supported = supported_protocols(provider);
    let current = (!provider.disabled)
        .then(|| ProviderCurrentConfig::new(primary_protocol(provider), provider.base_url.clone()));
    let mut meta = Map::new();
    meta.insert(
        "label".to_string(),
        json!(provider.label.clone().unwrap_or_else(|| id.to_string())),
    );
    meta.insert("models".to_string(), json!(provider.models));
    meta.insert(
        "apiKeyMasked".to_string(),
        json!(mask_secret(&provider.api_key)),
    );
    meta.insert(
        "hasApiKey".to_string(),
        json!(!provider.api_key.trim().is_empty()),
    );
    meta.insert("disabled".to_string(), json!(provider.disabled));
    meta.insert("readOnly".to_string(), json!(provider.env_var.is_some()));
    if let Some(var) = &provider.env_var {
        meta.insert("envVar".to_string(), json!(var));
    }
    let is_default = config.default_provider == id;
    meta.insert("isDefault".to_string(), json!(is_default));
    if is_default {
        meta.insert("defaultModel".to_string(), json!(config.default_model));
    }
    ProviderInfo::new(id.to_string(), supported, false, current).meta(meta)
}

/// 该 provider 的模型分布在哪些协议上（至少报一个，客户端拿它画协议徽标）。
fn supported_protocols(provider: &crate::model_config::ModelProviderConfig) -> Vec<LlmProtocol> {
    let mut out = Vec::new();
    if !provider.models.completions.is_empty() {
        out.push(LlmProtocol::OpenAi);
    }
    if !provider.models.messages.is_empty() {
        out.push(LlmProtocol::Anthropic);
    }
    if !provider.models.responses.is_empty() && out.is_empty() {
        out.push(LlmProtocol::OpenAi);
    }
    if out.is_empty() {
        out.push(LlmProtocol::OpenAi);
    }
    out
}

/// `current.apiType`：以 Anthropic 协议为主的 provider 报 `anthropic`，其余报 `openai`。
///
/// 单个模型真正的协议分组仍在 `_meta.models` 里，`current` 只是给不支持 `_meta` 的
/// 客户端一个可显示的值。
fn primary_protocol(provider: &crate::model_config::ModelProviderConfig) -> LlmProtocol {
    if provider.models.completions.is_empty()
        && provider.models.responses.is_empty()
        && !provider.models.messages.is_empty()
    {
        LlmProtocol::Anthropic
    } else {
        LlmProtocol::OpenAi
    }
}

/// `providers/set`：新增/改写一条，结果落盘。
///
/// 密钥取 `headers`（`Authorization: Bearer …` 或 `x-api-key`）；没带就保留原密钥，
/// 这样客户端编辑 base_url / 模型表时不必回填密钥。
pub fn set(request: SetProviderRequest) -> Result<(), String> {
    let id = request.provider_id.to_string();
    let meta = request.meta.unwrap_or_default();
    let models = match meta.get("models") {
        Some(value) => Some(
            serde_json::from_value::<ModelsSpec>(value.clone())
                .map_err(|error| format!("invalid models: {error}"))?,
        ),
        None => None,
    };
    let label = meta
        .get("label")
        .and_then(Value::as_str)
        .map(str::to_string);
    let disabled = meta.get("disabled").and_then(Value::as_bool);

    let mut config = load();
    config.upsert_provider(ProviderUpsert {
        id,
        label,
        base_url: request.base_url,
        api_key: api_key_from_headers(&request.headers),
        models,
        disabled,
    })?;
    config.save();
    Ok(())
}

/// 从标准面的 headers 里取密钥（`Bearer` 前缀去掉）。
///
/// schema 1.22 起 `SetProviderRequest.headers` 是 `IndexMap`（保留客户端顺序），
/// 旧调用点传 `HashMap`，所以这里只要求可迭代的键值对。
fn api_key_from_headers<S: AsRef<str>>(
    headers: impl IntoIterator<Item = (S, S)>,
) -> Option<String> {
    for (name, value) in headers {
        let name = name.as_ref().to_ascii_lowercase();
        if name != "authorization" && name != "x-api-key" {
            continue;
        }
        let value = value.as_ref().trim();
        let key = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
            .unwrap_or(value)
            .trim();
        if !key.is_empty() {
            return Some(key.to_string());
        }
    }
    None
}

/// `providers/disable`：停用它（保留密钥与模型表，可再启用）。
pub fn disable(id: &str) -> Result<(), String> {
    let mut config = load();
    config.set_disabled(id, true)?;
    config.save();
    Ok(())
}

/// `_acp_extends/providers/remove`：删除。
pub fn remove(id: &str) -> Result<(), String> {
    let mut config = load();
    config.remove_provider(id)?;
    config.save();
    Ok(())
}

/// `_acp_extends/providers/default`：改默认 provider / 模型（新会话用它）。
pub fn set_default(provider: &str, model: Option<&str>) -> Result<(), String> {
    let mut config = load();
    config.set_default(provider, model)?;
    config.save();
    Ok(())
}

/// `_acp_extends/providers/catalog`：models.dev 预设。
///
/// 不带 `provider_id` 时只给摘要（id / baseUrl / 协议 / 模型数），带去时才带上模型表，
/// 免得一次请求把整张目录（十几 MB 解析结果的模型名）全塞进 JSON-RPC。
pub fn catalog(provider_id: Option<&str>) -> Vec<Value> {
    let catalog = agentscope_core::model::catalog::ModelCatalog::global();
    if !catalog.ensure_loaded() {
        // 目录还没能拉到（离线首启）：客户端只显示“自定义供应商”。
        return Vec::new();
    }
    let providers = crate::setup::catalog_providers(&catalog.snapshot());
    let mut out = Vec::new();
    for provider in providers {
        if let Some(wanted) = provider_id {
            if !provider.id.eq_ignore_ascii_case(wanted) {
                continue;
            }
        }
        let mut entry = json!({
            "id": provider.id,
            "name": provider.id,
            "baseUrl": provider.base_url,
            "protocol": protocol_key(provider.protocol),
            "env": provider.env,
            "modelCount": provider.models.len(),
        });
        if provider_id.is_some() {
            entry["models"] = json!(provider.models);
        }
        out.push(entry);
    }
    out
}

fn protocol_key(protocol: ApiProtocol) -> &'static str {
    match protocol {
        ApiProtocol::ChatCompletions => "completions",
        ApiProtocol::Messages => "messages",
        ApiProtocol::Responses => "responses",
    }
}

/// `_acp_extends/providers/probe`：用已存（或本次传入）的端点与密钥拉模型列表。
///
/// 成功：`{ok: true, models: [id...], protocol}`；失败：`{ok: false, error}`。
/// 一个 200 但不带 `data` 数组的响应（某些网关只认 `/chat/completions`）也算成功，
/// 只是模型表为空——它证明端点与密钥是通的。
pub async fn probe(params: ExtParams) -> Value {
    let config = load();
    let saved = params
        .provider_id
        .as_deref()
        .and_then(|id| config.providers.get(id));

    let base_url = match params
        .base_url
        .clone()
        .or_else(|| saved.map(|provider| provider.base_url.clone()))
    {
        Some(url) if !url.trim().is_empty() => url.trim().trim_end_matches('/').to_string(),
        _ => return json!({"ok": false, "error": "a base URL is required"}),
    };
    let api_key = params
        .api_key
        .clone()
        .or_else(|| saved.map(|provider| provider.api_key.clone()))
        .unwrap_or_default();
    let protocol = params
        .protocol
        .clone()
        .or_else(|| {
            params.provider_id.as_deref().and_then(|id| {
                config
                    .providers
                    .get(id)
                    .map(|provider| protocol_key(primary_api(provider)).to_string())
            })
        })
        .unwrap_or_else(|| "completions".to_string());

    let url = format!("{base_url}/models");
    let mut headers = Vec::new();
    if !api_key.trim().is_empty() {
        if protocol == "messages" {
            headers.push(("x-api-key".to_string(), api_key.clone()));
            headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
        } else {
            headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
        }
    }

    let transport = ReqwestTransport::new();
    match transport.get_json(&url, &headers, "provider-probe").await {
        Ok(body) => {
            let models: Vec<String> = body
                .get("data")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("id").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            json!({"ok": true, "models": models, "protocol": protocol})
        }
        Err(error) => json!({"ok": false, "error": error.to_string()}),
    }
}

fn primary_api(provider: &crate::model_config::ModelProviderConfig) -> ApiProtocol {
    if provider.models.completions.is_empty()
        && provider.models.responses.is_empty()
        && !provider.models.messages.is_empty()
    {
        ApiProtocol::Messages
    } else if provider.models.completions.is_empty() && !provider.models.responses.is_empty() {
        ApiProtocol::Responses
    } else {
        ApiProtocol::ChatCompletions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn the_extension_methods_are_namespaced() {
        for method in [REMOVE_METHOD, DEFAULT_METHOD, CATALOG_METHOD, PROBE_METHOD] {
            assert!(method.starts_with('_'), "{method} must be an ACP extension");
            assert!(ProvidersExtRequest::matches_method(method));
        }
        assert!(!ProvidersExtRequest::matches_method("providers/list"));
    }

    #[test]
    fn a_bearer_header_and_an_x_api_key_both_carry_the_secret() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer sk-abc".to_string());
        assert_eq!(api_key_from_headers(&headers).as_deref(), Some("sk-abc"));
        let mut headers = HashMap::new();
        headers.insert("x-api-key".to_string(), "sk-ant".to_string());
        assert_eq!(api_key_from_headers(&headers).as_deref(), Some("sk-ant"));
        assert_eq!(api_key_from_headers(HashMap::<String, String>::new()), None);
    }

    #[test]
    fn params_round_trip_through_the_wire_format() {
        let request = ProvidersExtRequest::parse_message(
            PROBE_METHOD,
            &json!({"providerId": "deepseek", "baseUrl": "https://x/v1"}),
        )
        .expect("parses");
        assert_eq!(request.method(), PROBE_METHOD);
        assert_eq!(request.params.provider_id.as_deref(), Some("deepseek"));
        assert_eq!(request.params.base_url.as_deref(), Some("https://x/v1"));
        assert!(ProvidersExtRequest::parse_message("providers/list", &json!({})).is_err());
    }
}
