//! Tests for `AtPathExpansionMiddleware` real `@path` expansion.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock, Msg, TextBlock};
use agentscope_core::middleware::{AgentInput, Middleware, Next};
use futures::stream::StreamExt;

use agentscope_harness::agent::middleware::AtPathExpansionMiddleware;
use agentscope_harness::filesystem::LocalFilesystem;

#[tokio::test]
async fn expands_at_path_in_user_message() {
    let dir = std::env::temp_dir().join(format!("as_atpath_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.md"), "secret content").unwrap();
    let fs: Arc<dyn agentscope_harness::filesystem::Filesystem> =
        Arc::new(LocalFilesystem::new(dir.clone()));
    let mw = AtPathExpansionMiddleware::new(Arc::clone(&fs));

    let input = AgentInput::new(vec![Msg::user("see @note.md please")]);
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
    let next: Next<AgentInput> = {
        let recorded = std::sync::Arc::clone(&recorded);
        Box::new(move |in2| {
            *recorded.lock().unwrap() = Some(in2);
            futures::stream::iter::<Vec<agentscope_core::error::Result<agentscope_core::event::AgentEvent>>>(vec![])
                .boxed()
        })
    };
    let _stream = mw.on_agent(&RuntimeContext::default(), input, next);
    let _ = _stream.collect::<Vec<_>>().await;

    let captured = recorded.lock().unwrap().take().unwrap();
    let text = match &captured.messages[0].content[0] {
        ContentBlock::Text(tb) => tb.text.clone(),
        _ => String::new(),
    };
    assert!(text.contains("secret content"), "got: {text}");
    assert!(text.contains("note.md"), "got: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn leaves_unresolved_token_untouched() {
    let dir = std::env::temp_dir().join(format!("as_atpath2_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fs: Arc<dyn agentscope_harness::filesystem::Filesystem> =
        Arc::new(LocalFilesystem::new(dir.clone()));
    let mw = AtPathExpansionMiddleware::new(Arc::clone(&fs));

    let input = AgentInput::new(vec![Msg::user("email me at foo@bar.com")]);
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
    let next: Next<AgentInput> = {
        let recorded = std::sync::Arc::clone(&recorded);
        Box::new(move |in2| {
            *recorded.lock().unwrap() = Some(in2);
            futures::stream::iter::<Vec<agentscope_core::error::Result<agentscope_core::event::AgentEvent>>>(vec![])
                .boxed()
        })
    };
    let _ = mw.on_agent(&RuntimeContext::default(), input, next).collect::<Vec<_>>().await;
    let captured = recorded.lock().unwrap().take().unwrap();
    let text = match &captured.messages[0].content[0] {
        ContentBlock::Text(tb) => tb.text.clone(),
        _ => String::new(),
    };
    assert_eq!(text, "email me at foo@bar.com", "got: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}
