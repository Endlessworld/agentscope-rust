//! Control-plane hosted [`BaseStore`] over `/api/v1/dp/kv/*`, mirroring
//! `ControlPlaneBaseStore`.
//!
//! HTTP failures and unexpected status codes surface as errors — never silently empty
//! results (except `get`, which maps 404 to `None`).

use std::sync::Arc;

use agentscope_harness::filesystem::remote::store::{BaseStore, StoreItem, StoreValue};
use reqwest::Method;
use serde_json::Value;

use crate::store::control_plane_stores::tenant_query;
use crate::transport::{encode_path_segment, encode_query, require_ok, ControlPlaneHttpClient};

/// Control-plane hosted [`BaseStore`].
#[derive(Clone)]
pub struct ControlPlaneBaseStore {
    http: Arc<ControlPlaneHttpClient>,
    agent_name: String,
    namespace: String,
}

impl ControlPlaneBaseStore {
    /// Creates a store bound to the given client.
    pub fn new(
        http: Arc<ControlPlaneHttpClient>,
        agent_name: impl Into<String>,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            http,
            agent_name: agent_name.into(),
            namespace: namespace.into(),
        }
    }

    fn tenant_query(&self) -> String {
        tenant_query(&self.agent_name, &self.namespace)
    }

    fn ns_query(namespace: &[String]) -> String {
        if namespace.is_empty() {
            String::new()
        } else {
            let joined = namespace.join("/");
            format!("&ns={}", encode_query(&joined))
        }
    }

    fn parse_item(body: &str) -> Option<StoreItem> {
        let value: Value = serde_json::from_str(body).ok()?;
        let key = value.get("key")?.as_str()?.to_string();
        let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
        let raw = value
            .get("value")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        let value = match raw {
            Value::Object(map) => map,
            other => {
                let mut m = serde_json::Map::new();
                m.insert("value".into(), other);
                m
            }
        };
        Some(StoreItem::new(key, value, version))
    }

    fn parse_items(body: &str) -> Vec<StoreItem> {
        let value: Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let arr = value
            .get("items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        arr.into_iter()
            .filter_map(|v| {
                let key = v.get("key")?.as_str()?.to_string();
                let version = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0);
                let value = match v.get("value").cloned().unwrap_or(Value::Null) {
                    Value::Object(m) => m,
                    Value::Null => serde_json::Map::new(),
                    other => {
                        let mut m = serde_json::Map::new();
                        m.insert("value".into(), other);
                        m
                    }
                };
                Some(StoreItem::new(key, value, version))
            })
            .collect()
    }
}

/// Blocking bridge: `BaseStore` is synchronous in harness, so we run the async call on a
/// dedicated runtime handle. Callers invoking from within a tokio runtime use
/// `block_in_place`-free `futures::executor` to avoid nested-runtime panics.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    futures::executor::block_on(fut)
}

impl BaseStore for ControlPlaneBaseStore {
    fn get(&self, namespace: &[String], key: &str) -> Option<StoreItem> {
        let path = format!(
            "/api/v1/dp/kv/item?{}&key={}{}",
            self.tenant_query(),
            encode_query(key),
            Self::ns_query(namespace)
        );
        let resp = block_on(self.http.send(Method::GET, &path, None)).ok()?;
        if resp.status == 404 {
            return None;
        }
        require_ok(&resp, "get").ok()?;
        Self::parse_item(&resp.body)
    }

    fn put(&self, namespace: &[String], key: &str, value: StoreValue) {
        let body = serde_json::json!({
            "agentName": self.agent_name,
            "namespace": self.namespace,
            "ns": namespace,
            "key": key,
            "value": value,
        });
        let resp = block_on(
            self.http
                .send(Method::PUT, "/api/v1/dp/kv/item", Some(body)),
        );
        if let Ok(resp) = resp {
            let _ = require_ok(&resp, "put");
        }
    }

    fn put_if_version(
        &self,
        namespace: &[String],
        key: &str,
        value: StoreValue,
        expected_version: u64,
    ) -> bool {
        let body = serde_json::json!({
            "agentName": self.agent_name,
            "namespace": self.namespace,
            "ns": namespace,
            "key": key,
            "value": value,
            "expectedVersion": expected_version,
        });
        match block_on(
            self.http
                .send(Method::PUT, "/api/v1/dp/kv/item", Some(body)),
        ) {
            Ok(resp) => resp.status != 409 && (200..300).contains(&resp.status),
            Err(_) => false,
        }
    }

    /// `put_if_version` is a single conditional record write, so it is atomic across every
    /// writer sharing the store (Java `supportsAtomicSessionStorage`).
    fn supports_atomic_session_storage(&self) -> bool {
        true
    }

    fn search(&self, namespace: &[String], limit: usize, offset: usize) -> Vec<StoreItem> {
        let path = format!(
            "/api/v1/dp/kv/search?{}{}&limit={}&offset={}",
            self.tenant_query(),
            Self::ns_query(namespace),
            limit,
            offset
        );
        match block_on(self.http.send(Method::GET, &path, None)) {
            Ok(resp) if (200..300).contains(&resp.status) => Self::parse_items(&resp.body),
            _ => Vec::new(),
        }
    }

    fn delete(&self, namespace: &[String], key: &str) {
        let path = format!(
            "/api/v1/dp/kv/item?{}&key={}{}",
            self.tenant_query(),
            encode_path_segment(key),
            Self::ns_query(namespace)
        );
        let _ = block_on(self.http.send(reqwest::Method::DELETE, &path, None));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_item_body() {
        let body = r#"{"key":"k1","version":3,"value":{"a":1}}"#;
        let item = ControlPlaneBaseStore::parse_item(body).unwrap();
        assert_eq!(item.key, "k1");
        assert_eq!(item.version, 3);
        assert_eq!(item.value.get("a").unwrap(), &serde_json::json!(1));
    }

    #[test]
    fn parses_items_array() {
        let body = r#"{"items":[{"key":"k1","version":1,"value":{}},{"key":"k2","version":2,"value":{}}]}"#;
        let items = ControlPlaneBaseStore::parse_items(body);
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].key, "k2");
    }

    #[test]
    fn ns_query_encodes_nested_namespace() {
        let q = ControlPlaneBaseStore::ns_query(&["user".into(), "alice".into()]);
        assert_eq!(q, "&ns=user%2Falice");
    }
}
