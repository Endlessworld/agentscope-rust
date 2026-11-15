//! Wire-level tests: the bytes this crate writes, and what it accepts.

use agentscope_durable_mcp::jsonrpc::codes;
use agentscope_durable_mcp::{
    initialize_request, initialized_notification, tools_call_request, tools_list_page_request,
    tools_list_request, ErrorObject, Method, Notification, Request, RequestId, Response,
    CLIENT_NAME, CLIENT_VERSION, JSONRPC_VERSION, PROTOCOL_VERSION,
};
use serde_json::json;

const GOLDEN_INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"agentscope-durable-mcp","version":"__VERSION__"}}}"#;

#[test]
fn the_initialize_request_is_byte_for_byte_the_golden_message() {
    let request = initialize_request(RequestId::number(1)).expect("initialize");
    let expected = GOLDEN_INITIALIZE.replace("__VERSION__", CLIENT_VERSION);

    // Sanity: the golden text really is what the constants say it is.
    assert_eq!(PROTOCOL_VERSION, "2025-06-18");
    assert_eq!(CLIENT_NAME, "agentscope-durable-mcp");

    assert_eq!(request.to_wire().expect("wire"), expected);
}

#[test]
fn the_tools_call_request_is_byte_for_byte_the_golden_message() {
    let request = tools_call_request(7, "echo", &json!({ "text": "hi" })).expect("tools/call");
    assert_eq!(
        request.to_wire().expect("wire"),
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hi"}}}"#
    );
}

#[test]
fn the_initialized_notification_is_byte_for_byte_the_golden_message() {
    let notification = initialized_notification();
    assert_eq!(
        notification.to_wire().expect("wire"),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
    );
}

#[test]
fn the_tools_list_request_is_byte_for_byte_the_golden_message() {
    let request = tools_list_request(RequestId::number(2));
    assert_eq!(
        request.to_wire().expect("wire"),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#
    );
}

#[test]
fn a_response_round_trips_through_the_wire_in_both_shapes() {
    let success = Response::result(RequestId::number(4), json!({ "tools": [] }));
    let wire = success.to_wire().expect("wire");
    assert_eq!(wire, r#"{"jsonrpc":"2.0","id":4,"result":{"tools":[]}}"#);
    let parsed = Response::parse(&wire).expect("parse");
    assert_eq!(parsed, success);
    assert_eq!(parsed.to_wire().expect("re-wire"), wire);

    let failure = Response::with_error(
        Some(RequestId::number(3)),
        ErrorObject::new(codes::METHOD_NOT_FOUND, "Method not found"),
    );
    let wire = failure.to_wire().expect("wire");
    assert_eq!(
        wire,
        r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"Method not found"}}"#
    );
    let parsed = Response::parse(&wire).expect("parse");
    assert_eq!(parsed, failure);
    assert_eq!(parsed.to_wire().expect("re-wire"), wire);
}

#[test]
fn requests_and_notifications_survive_decode_and_re_encode_unchanged() {
    let request = initialize_request(RequestId::text("req-1")).expect("initialize");
    let wire = request.to_wire().expect("wire");
    let parsed = Request::parse(&wire).expect("parse");
    assert_eq!(parsed, request);
    assert_eq!(parsed.to_wire().expect("re-wire"), wire);
    assert_eq!(parsed.id, RequestId::Text("req-1".to_owned()));
    assert_eq!(parsed.jsonrpc, JSONRPC_VERSION);

    let notification = Notification::new("notifications/tools/list_changed");
    let wire = notification.to_wire().expect("wire");
    assert_eq!(Notification::parse(&wire).expect("parse"), notification);
}

#[test]
fn params_are_read_back_from_the_raw_wire_slice() {
    let request = tools_call_request(1, "echo", &json!({ "text": "hi" })).expect("call");
    assert_eq!(
        request.params_value().expect("params"),
        json!({ "name": "echo", "arguments": { "text": "hi" } })
    );

    let typed: agentscope_durable_mcp::ToolsListParams = tools_list_page_request(2, "page-2")
        .expect("page")
        .params_as()
        .expect("typed params");
    assert_eq!(typed.cursor, "page-2");
}

#[test]
fn garbage_is_rejected_as_a_parse_error() {
    let error = Request::parse("{\"jsonrpc\":").expect_err("must be rejected");
    assert_eq!(error.code(), Some(codes::PARSE_ERROR));
    assert_eq!(
        agentscope_durable_mcp::classify(codes::PARSE_ERROR).name,
        "PARSE_ERROR"
    );
}

#[test]
fn well_formed_json_that_is_not_a_jsonrpc_message_is_an_invalid_request() {
    for text in [
        r#"{"id":1,"method":"initialize"}"#,
        r#"{"jsonrpc":"1.0","id":1,"method":"initialize"}"#,
        r#"{"jsonrpc":"2.0","id":1.5,"method":"initialize"}"#,
        r#"{"jsonrpc":"2.0","id":1}"#,
        r#"[]"#,
    ] {
        let error = Request::parse(text).expect_err("must be rejected");
        assert_eq!(error.code(), Some(codes::INVALID_REQUEST), "text: {text}");
    }
}

#[test]
fn replies_that_carry_no_or_two_payloads_are_rejected() {
    for wire in [
        r#"{"jsonrpc":"2.0","id":1}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-32603,"message":"x"}}"#,
    ] {
        let error = Response::parse(wire).expect_err("must be rejected");
        assert_eq!(error.code(), Some(codes::INVALID_REQUEST), "wire: {wire}");
    }
}

#[test]
fn unknown_methods_are_rejected_instead_of_ignored() {
    let error = Method::parse("tools/poke").expect_err("must be rejected");
    assert_eq!(error.code, codes::METHOD_NOT_FOUND);
    assert_eq!(
        Method::parse("tools/call").expect("known").name(),
        "tools/call"
    );
}
