//! Security scheme conversion between Nacos and the A2A specification,
//! mirroring `io.agentscope.core.nacos.a2a.utils.SecuritySchemeConverterUtil`.
//!
//! Nacos stores A2A security schemes as maps in the A2A 0.3 format, with the
//! scheme kind in `type`. Both sides are JSON here (the protocol model types
//! `SecurityScheme` as `serde_json::Value`), so the converter is the dispatch that
//! Java performs against the typed `io.a2a.spec` classes:
//!
//! | `type` | fields |
//! |---|---|
//! | `apiKey` | `in` (`query`/`header`/`cookie`), `name`, `description` |
//! | `http` | `scheme`, `bearerFormat`, `description` |
//! | `oauth2` | `flows` (`authorizationCode` / `clientCredentials` / `deviceCode`), `oauth2MetadataUrl`, `description` |
//! | `openIdConnect` | `openIdConnectUrl`, `description` |
//! | `mutualTLS` | `description` |
//!
//! Unknown types are **dropped** when reading from Nacos (Java returns `null` for
//! a scheme it cannot describe), and collapse to an empty map when writing.

use serde_json::{Map, Value};

const TYPE: &str = "type";
const DESCRIPTION: &str = "description";

/// Locations accepted by `io.a2a.spec.APIKeySecurityScheme.Location`.
const API_KEY_LOCATIONS: [&str; 3] = ["query", "header", "cookie"];

/// Converts the Nacos `securitySchemes` map into its A2A view.
///
/// Returns `None` when the input is absent (Java returns `null`); schemes of an
/// unknown `type` (or an `apiKey` with an unrecognised `in`) are omitted.
pub fn convert_to_a2a_security_schemes(
    security_schemes: Option<&Map<String, Value>>,
) -> Option<Map<String, Value>> {
    let security_schemes = security_schemes?;
    let mut result = Map::with_capacity(security_schemes.len());
    for (name, scheme) in security_schemes {
        if let Some(converted) = convert_to_a2a_security_scheme(scheme) {
            result.insert(name.clone(), converted);
        }
    }
    Some(result)
}

/// Converts the A2A `securitySchemes` map into its Nacos (A2A 0.3) form.
///
/// Returns `None` when the input is absent; every scheme is kept, a scheme of an
/// unknown `type` becoming an empty map, mirroring Java's
/// `convertToNacosSecuritySchemes`.
pub fn convert_to_nacos_security_schemes(
    security_schemes: Option<&Map<String, Value>>,
) -> Option<Map<String, Value>> {
    let security_schemes = security_schemes?;
    let mut result = Map::with_capacity(security_schemes.len());
    for (name, scheme) in security_schemes {
        result.insert(name.clone(), convert_to_nacos_security_scheme(scheme));
    }
    Some(result)
}

/// Dispatches one A2A 0.3 scheme map onto the typed A2A shape.
///
/// `None` means "not representable", which is exactly when Java returns `null`
/// (`Type`/`location` checks).
fn convert_to_a2a_security_scheme(scheme: &Value) -> Option<Value> {
    let map = scheme.as_object()?;
    // Java `String.valueOf(scheme.get(TYPE))`: a missing type reads as `"null"`
    // and falls through to the `default` arm.
    let scheme_type = match map.get(TYPE) {
        None | Some(Value::Null) => "null".to_string(),
        Some(Value::String(value)) => value.clone(),
        Some(other) => other.to_string(),
    };
    let mut result = Map::new();
    match scheme_type.as_str() {
        "apiKey" => {
            let location = string(map.get("in"))
                .filter(|location| API_KEY_LOCATIONS.contains(&location.as_str()))?;
            result.insert(TYPE.to_string(), Value::String("apiKey".to_string()));
            result.insert("in".to_string(), Value::String(location));
            put_string(&mut result, "name", string(map.get("name")));
            put_string(&mut result, DESCRIPTION, string(map.get(DESCRIPTION)));
        }
        "http" => {
            result.insert(TYPE.to_string(), Value::String("http".to_string()));
            put_string(&mut result, "scheme", string(map.get("scheme")));
            put_string(&mut result, "bearerFormat", string(map.get("bearerFormat")));
            put_string(&mut result, DESCRIPTION, string(map.get(DESCRIPTION)));
        }
        "oauth2" => {
            result.insert(TYPE.to_string(), Value::String("oauth2".to_string()));
            result.insert(
                "flows".to_string(),
                convert_to_a2a_oauth_flows(map.get("flows")),
            );
            put_string(
                &mut result,
                "oauth2MetadataUrl",
                string(map.get("oauth2MetadataUrl")),
            );
            put_string(&mut result, DESCRIPTION, string(map.get(DESCRIPTION)));
        }
        "openIdConnect" => {
            result.insert(TYPE.to_string(), Value::String("openIdConnect".to_string()));
            put_string(
                &mut result,
                "openIdConnectUrl",
                string(map.get("openIdConnectUrl")),
            );
            put_string(&mut result, DESCRIPTION, string(map.get(DESCRIPTION)));
        }
        "mutualTLS" => {
            result.insert(TYPE.to_string(), Value::String("mutualTLS".to_string()));
            put_string(&mut result, DESCRIPTION, string(map.get(DESCRIPTION)));
        }
        _ => return None,
    }
    Some(Value::Object(result))
}

/// OAuth flows readable by A2A 1.0 (implicit / password are gone).
fn convert_to_a2a_oauth_flows(flows: Option<&Value>) -> Value {
    let mut result = Map::new();
    let Some(flows) = flows.and_then(Value::as_object) else {
        return Value::Object(result);
    };

    if let Some(code) = flows.get("authorizationCode").and_then(Value::as_object) {
        let mut flow = Map::new();
        put_string(
            &mut flow,
            "authorizationUrl",
            string(code.get("authorizationUrl")),
        );
        put_string(&mut flow, "refreshUrl", string(code.get("refreshUrl")));
        flow.insert("scopes".to_string(), scopes(code));
        put_string(&mut flow, "tokenUrl", string(code.get("tokenUrl")));
        flow.insert(
            "pkceRequired".to_string(),
            Value::Bool(matches!(code.get("pkceRequired"), Some(Value::Bool(true)))),
        );
        result.insert("authorizationCode".to_string(), Value::Object(flow));
    }

    if let Some(client) = flows.get("clientCredentials").and_then(Value::as_object) {
        let mut flow = Map::new();
        put_string(&mut flow, "refreshUrl", string(client.get("refreshUrl")));
        flow.insert("scopes".to_string(), scopes(client));
        put_string(&mut flow, "tokenUrl", string(client.get("tokenUrl")));
        result.insert("clientCredentials".to_string(), Value::Object(flow));
    }

    if let Some(device) = flows.get("deviceCode").and_then(Value::as_object) {
        let mut flow = Map::new();
        put_string(
            &mut flow,
            "deviceAuthorizationUrl",
            string(device.get("deviceAuthorizationUrl")),
        );
        put_string(&mut flow, "tokenUrl", string(device.get("tokenUrl")));
        put_string(&mut flow, "refreshUrl", string(device.get("refreshUrl")));
        flow.insert("scopes".to_string(), scopes(device));
        result.insert("deviceCode".to_string(), Value::Object(flow));
    }

    Value::Object(result)
}

/// Dispatches one A2A scheme JSON map back to the A2A 0.3 (Nacos) shape.
fn convert_to_nacos_security_scheme(scheme: &Value) -> Value {
    let mut result = Map::new();
    let Some(map) = scheme.as_object() else {
        return Value::Object(result);
    };
    // Java dispatches on the typed scheme class; an unknown `type` falls through
    // every `instanceof` branch and yields the empty map.
    match map.get(TYPE).and_then(Value::as_str) {
        Some("apiKey") => {
            result.insert(TYPE.to_string(), Value::String("apiKey".to_string()));
            put_value(&mut result, "name", map.get("name"));
            put_value(&mut result, "in", map.get("in"));
            put_value(&mut result, DESCRIPTION, map.get(DESCRIPTION));
        }
        Some("http") => {
            result.insert(TYPE.to_string(), Value::String("http".to_string()));
            put_value(&mut result, "scheme", map.get("scheme"));
            put_value(&mut result, "bearerFormat", map.get("bearerFormat"));
            put_value(&mut result, DESCRIPTION, map.get(DESCRIPTION));
        }
        Some("oauth2") => {
            result.insert(TYPE.to_string(), Value::String("oauth2".to_string()));
            result.insert(
                "flows".to_string(),
                convert_to_nacos_oauth_flows(map.get("flows")),
            );
            put_value(
                &mut result,
                "oauth2MetadataUrl",
                map.get("oauth2MetadataUrl"),
            );
            put_value(&mut result, DESCRIPTION, map.get(DESCRIPTION));
        }
        Some("openIdConnect") => {
            result.insert(TYPE.to_string(), Value::String("openIdConnect".to_string()));
            put_value(&mut result, "openIdConnectUrl", map.get("openIdConnectUrl"));
            put_value(&mut result, DESCRIPTION, map.get(DESCRIPTION));
        }
        Some("mutualTLS") => {
            result.insert(TYPE.to_string(), Value::String("mutualTLS".to_string()));
            put_value(&mut result, DESCRIPTION, map.get(DESCRIPTION));
        }
        _ => {}
    }
    Value::Object(result)
}

/// Serialises typed OAuth flows back into the A2A 0.3 map shape.
///
/// `flows` is always present (an empty map when there is nothing to describe),
/// mirroring Java's `convertToNacosOAuthFlows`, which starts from an empty map.
fn convert_to_nacos_oauth_flows(flows: Option<&Value>) -> Value {
    let mut result = Map::new();
    let Some(flows) = flows.and_then(Value::as_object) else {
        return Value::Object(result);
    };

    if let Some(code) = flows.get("authorizationCode").and_then(Value::as_object) {
        let mut flow = Map::new();
        put_value(&mut flow, "authorizationUrl", code.get("authorizationUrl"));
        put_value(&mut flow, "tokenUrl", code.get("tokenUrl"));
        put_value(&mut flow, "refreshUrl", code.get("refreshUrl"));
        flow.insert("scopes".to_string(), scopes(code));
        flow.insert(
            "pkceRequired".to_string(),
            Value::Bool(matches!(code.get("pkceRequired"), Some(Value::Bool(true)))),
        );
        result.insert("authorizationCode".to_string(), Value::Object(flow));
    }

    if let Some(client) = flows.get("clientCredentials").and_then(Value::as_object) {
        let mut flow = Map::new();
        put_value(&mut flow, "tokenUrl", client.get("tokenUrl"));
        put_value(&mut flow, "refreshUrl", client.get("refreshUrl"));
        flow.insert("scopes".to_string(), scopes(client));
        result.insert("clientCredentials".to_string(), Value::Object(flow));
    }

    if let Some(device) = flows.get("deviceCode").and_then(Value::as_object) {
        let mut flow = Map::new();
        put_value(
            &mut flow,
            "deviceAuthorizationUrl",
            device.get("deviceAuthorizationUrl"),
        );
        put_value(&mut flow, "tokenUrl", device.get("tokenUrl"));
        put_value(&mut flow, "refreshUrl", device.get("refreshUrl"));
        flow.insert("scopes".to_string(), scopes(device));
        result.insert("deviceCode".to_string(), Value::Object(flow));
    }

    Value::Object(result)
}

/// `scopes` is an object of `name -> description`; a missing / non-object value
/// becomes an empty object (Java `Map.of()`).
fn scopes(flow: &Map<String, Value>) -> Value {
    match flow.get("scopes") {
        Some(Value::Object(scopes)) => Value::Object(scopes.clone()),
        _ => Value::Object(Map::new()),
    }
}

/// Java `string(map, key)`: the value's string form, or `None`.
fn string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Java `putIfPresent`.
fn put_string(map: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_string(), Value::String(value));
    }
}

/// Java `putIfPresent` for an arbitrary (non-null) value.
fn put_value(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(value) = value.filter(|value| !value.is_null()) {
        map.insert(key.to_string(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schemes(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn absent_schemes_stay_absent() {
        assert!(convert_to_a2a_security_schemes(None).is_none());
        assert!(convert_to_nacos_security_schemes(None).is_none());
    }

    #[test]
    fn reads_every_supported_type() {
        let input = schemes(json!({
            "apiKey": {"type": "apiKey", "in": "header", "name": "X-API-Key"},
            "bearer": {"type": "http", "scheme": "bearer", "bearerFormat": "JWT"},
            "oidc": {"type": "openIdConnect", "openIdConnectUrl": "https://auth/oidc"},
            "mtls": {"type": "mutualTLS", "description": "Client certificate"}
        }));
        let converted = convert_to_a2a_security_schemes(Some(&input)).unwrap();
        assert_eq!(converted.len(), 4);
        assert_eq!(
            converted["apiKey"],
            json!({"type": "apiKey", "in": "header", "name": "X-API-Key"})
        );
        assert_eq!(
            converted["bearer"],
            json!({"type": "http", "scheme": "bearer", "bearerFormat": "JWT"})
        );
        assert_eq!(
            converted["mtls"],
            json!({"type": "mutualTLS", "description": "Client certificate"})
        );
    }

    #[test]
    fn unknown_type_and_bad_api_key_location_are_dropped() {
        // Mirrors Java `testConvertToA2aAgentCardWithLegacySecuritySchemes`.
        let input = schemes(json!({
            "unknown": {"type": "unknown"},
            "typeless": {"in": "header"},
            "badLocation": {"type": "apiKey", "in": "body", "name": "k"},
            "noLocation": {"type": "apiKey", "name": "k"}
        }));
        let converted = convert_to_a2a_security_schemes(Some(&input)).unwrap();
        assert!(converted.is_empty(), "ACTUAL: {converted:?}");
    }

    #[test]
    fn oauth_flows_keep_only_the_typed_flows() {
        let input = schemes(json!({
            "oauth": {
                "type": "oauth2",
                "flows": {
                    "clientCredentials": {"tokenUrl": "https://auth/token", "scopes": {}},
                    "implicit": {"authorizationUrl": "https://auth/authorize"}
                }
            }
        }));
        let converted = convert_to_a2a_security_schemes(Some(&input)).unwrap();
        assert_eq!(
            converted["oauth"],
            json!({
                "type": "oauth2",
                "flows": {
                    "clientCredentials": {"scopes": {}, "tokenUrl": "https://auth/token"}
                }
            })
        );
    }

    #[test]
    fn oauth_flows_are_normalised() {
        let input = schemes(json!({
            "oauth": {
                "type": "oauth2",
                "flows": {
                    "authorizationCode": {
                        "authorizationUrl": "https://auth/authorize",
                        "tokenUrl": "https://auth/token",
                        "refreshUrl": "https://auth/refresh",
                        "scopes": {"read": "Read"},
                        "pkceRequired": true
                    },
                    "deviceCode": {
                        "deviceAuthorizationUrl": "https://auth/device",
                        "tokenUrl": "https://auth/token"
                    }
                },
                "oauth2MetadataUrl": "https://auth/.well-known/oauth-authorization-server"
            }
        }));
        let converted = convert_to_a2a_security_schemes(Some(&input)).unwrap();
        assert_eq!(
            converted["oauth"],
            json!({
                "type": "oauth2",
                "flows": {
                    "authorizationCode": {
                        "authorizationUrl": "https://auth/authorize",
                        "refreshUrl": "https://auth/refresh",
                        "scopes": {"read": "Read"},
                        "tokenUrl": "https://auth/token",
                        "pkceRequired": true
                    },
                    "deviceCode": {
                        "deviceAuthorizationUrl": "https://auth/device",
                        "scopes": {},
                        "tokenUrl": "https://auth/token"
                    }
                },
                "oauth2MetadataUrl": "https://auth/.well-known/oauth-authorization-server"
            })
        );
    }

    #[test]
    fn writes_the_a2a_03_shape() {
        let input = schemes(json!({
            "apiKey": {"type": "apiKey", "in": "query", "name": "api_key", "description": "Query key"},
            "mtls": {"type": "mutualTLS"},
            "unknown": {"type": "custom"}
        }));
        let converted = convert_to_nacos_security_schemes(Some(&input)).unwrap();
        assert_eq!(
            converted["apiKey"],
            json!({"type": "apiKey", "in": "query", "name": "api_key", "description": "Query key"})
        );
        assert_eq!(converted["mtls"], json!({"type": "mutualTLS"}));
        // A scheme Java cannot type collapses to an empty map.
        assert_eq!(converted["unknown"], json!({}));
    }

    #[test]
    fn round_trips_every_scheme_kind() {
        // Mirrors Java `testSecuritySchemesRoundTrip`.
        let typed = schemes(json!({
            "apiKey": {"type": "apiKey", "in": "query", "name": "api_key", "description": "Query key"},
            "bearer": {"type": "http", "scheme": "bearer", "bearerFormat": "JWT"},
            "oauth": {
                "type": "oauth2",
                "flows": {
                    "authorizationCode": {
                        "authorizationUrl": "https://auth/authorize",
                        "refreshUrl": "https://auth/refresh",
                        "scopes": {"read": "Read"},
                        "tokenUrl": "https://auth/token",
                        "pkceRequired": true
                    },
                    "clientCredentials": {"scopes": {}, "tokenUrl": "https://auth/token"},
                    "deviceCode": {
                        "deviceAuthorizationUrl": "https://auth/device",
                        "scopes": {"read": "Read"},
                        "tokenUrl": "https://auth/token"
                    }
                },
                "description": "OAuth",
                "oauth2MetadataUrl": "https://auth/.well-known/oauth-authorization-server"
            },
            "oidc": {"type": "openIdConnect", "openIdConnectUrl": "https://auth/.well-known/openid-configuration"},
            "mtls": {"type": "mutualTLS", "description": "Client certificate"}
        }));
        let nacos = convert_to_nacos_security_schemes(Some(&typed)).unwrap();
        let back = convert_to_a2a_security_schemes(Some(&nacos)).unwrap();
        assert_eq!(back, typed);
    }

    #[test]
    fn oauth2_without_flows_gets_an_empty_flows_object() {
        let input = schemes(json!({"oauth": {"type": "oauth2"}}));
        let converted = convert_to_a2a_security_schemes(Some(&input)).unwrap();
        assert_eq!(converted["oauth"], json!({"type": "oauth2", "flows": {}}));
    }
}
