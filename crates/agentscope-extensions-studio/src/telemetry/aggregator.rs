//! Aggregates a stream of [`ChatResponse`] chunks into a single response,
//! mirroring `StreamChatResponseAggregator`.
use agentscope_core::message::{ChatUsage, ContentBlock, TextBlock, ThinkingBlock, ToolUseBlock};
use agentscope_core::model::ChatResponse;

/// Accumulates streaming chunks so response attributes can be recorded at the
/// end of a model call.
#[derive(Debug, Default)]
struct TextAccumulator {
    text: String,
}

impl TextAccumulator {
    fn add(&mut self, block: &TextBlock) {
        self.text.push_str(&block.text);
    }

    fn build_aggregated(&self) -> ContentBlock {
        ContentBlock::Text(TextBlock::new(self.text.clone()))
    }
}

#[derive(Debug, Default)]
struct ThinkingAccumulator {
    thinking: String,
}

impl ThinkingAccumulator {
    fn add(&mut self, block: &ThinkingBlock) {
        self.thinking.push_str(&block.thinking);
    }

    fn build_aggregated(&self) -> ContentBlock {
        ContentBlock::Thinking(ThinkingBlock::new(self.thinking.clone()))
    }
}

/// Accumulates tool calls, merging fragments by index/id.
#[derive(Debug, Default)]
struct ToolCallsAccumulator {
    calls: Vec<ToolUseBlock>,
}

impl ToolCallsAccumulator {
    fn add(&mut self, block: &ToolUseBlock) {
        // Match on id when present; otherwise append a new call.
        if let Some(existing) = self
            .calls
            .iter_mut()
            .find(|c| !c.id.is_empty() && c.id == block.id)
        {
            if let Some(content) = &block.content {
                let merged = existing.content.take().unwrap_or_default() + content;
                existing.content = Some(merged);
            }
            if existing.name.is_empty() {
                existing.name = block.name.clone();
            }
            for (k, v) in &block.input {
                existing.input.insert(k.clone(), v.clone());
            }
        } else {
            self.calls.push(block.clone());
        }
    }

    fn build_all_tool_calls(&self) -> Vec<ToolUseBlock> {
        self.calls.clone()
    }
}

/// Aggregates streaming model responses.
#[derive(Debug, Default)]
pub struct StreamChatResponseAggregator {
    id: Option<String>,
    text_acc: TextAccumulator,
    thinking_acc: ThinkingAccumulator,
    tool_calls_acc: ToolCallsAccumulator,
    input_tokens: u64,
    output_tokens: u64,
    time: f64,
    finish_reason: Option<String>,
}

impl StreamChatResponseAggregator {
    /// Creates an empty aggregator.
    pub fn create() -> Self {
        Self::default()
    }

    /// Appends a single streaming chunk.
    pub fn append(&mut self, chunk: &ChatResponse) {
        if !chunk.id.is_empty() {
            self.id = Some(chunk.id.clone());
        }

        for block in &chunk.content {
            match block {
                ContentBlock::Text(tb) => self.text_acc.add(tb),
                ContentBlock::Thinking(tb) => self.thinking_acc.add(tb),
                ContentBlock::ToolUse(tub) => self.tool_calls_acc.add(tub),
                _ => {}
            }
        }

        // Providers report cumulative totals, so take the max of each chunk.
        self.input_tokens = self.input_tokens.max(chunk.usage.input_tokens);
        self.output_tokens = self.output_tokens.max(chunk.usage.output_tokens);
        if chunk.usage.time != 0.0 {
            self.time = chunk.usage.time;
        }

        if let Some(reason) = &chunk.finish_reason {
            self.finish_reason = Some(reason.clone());
        }
    }

    /// Builds the aggregated response.
    pub fn response(&self) -> ChatResponse {
        let tool_use_blocks = self.tool_calls_acc.build_all_tool_calls();
        let mut content_blocks = Vec::with_capacity(tool_use_blocks.len() + 2);
        content_blocks.push(self.text_acc.build_aggregated());
        content_blocks.push(self.thinking_acc.build_aggregated());
        content_blocks.extend(tool_use_blocks.into_iter().map(ContentBlock::ToolUse));

        ChatResponse {
            id: self.id.clone().unwrap_or_default(),
            content: content_blocks,
            usage: ChatUsage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cached_tokens: 0,
                time: self.time,
                ..Default::default()
            },
            metadata: Default::default(),
            finish_reason: self.finish_reason.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text(TextBlock::new(text))
    }

    fn tool_use(id: &str, name: &str, content: &str) -> ContentBlock {
        ContentBlock::ToolUse(
            ToolUseBlock::builder()
                .id(id)
                .name(name)
                .content(content)
                .build(),
        )
    }

    #[test]
    fn aggregates_text_chunks_in_order() {
        let mut agg = StreamChatResponseAggregator::create();
        agg.append(&ChatResponse::new(
            "",
            vec![text_block("Hel")],
            ChatUsage::default(),
            None,
            None,
        ));
        agg.append(&ChatResponse::new(
            "resp-1",
            vec![text_block("lo")],
            ChatUsage::builder()
                .input_tokens(3)
                .output_tokens(2)
                .build(),
            None,
            Some("stop".to_string()),
        ));
        let resp = agg.response();
        assert_eq!(resp.id, "resp-1");
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.usage.input_tokens, 3);
        assert_eq!(resp.usage.output_tokens, 2);
        assert_eq!(resp.content[0], text_block("Hello"));
    }

    #[test]
    fn merges_tool_call_fragments_by_id() {
        let mut agg = StreamChatResponseAggregator::create();
        agg.append(&ChatResponse::new(
            "",
            vec![tool_use("call_1", "calc", "{\"a\"")],
            ChatUsage::default(),
            None,
            None,
        ));
        agg.append(&ChatResponse::new(
            "",
            vec![tool_use("call_1", "", ":1}")],
            ChatUsage::default(),
            None,
            None,
        ));
        let resp = agg.response();
        let tool_blocks: Vec<&ToolUseBlock> = resp
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tool_blocks.len(), 1);
        assert_eq!(tool_blocks[0].name, "calc");
        assert_eq!(tool_blocks[0].content.as_deref(), Some("{\"a\":1}"));
    }

    #[test]
    fn max_usage_and_time_are_kept() {
        let mut agg = StreamChatResponseAggregator::create();
        agg.append(&ChatResponse::new(
            "",
            vec![],
            ChatUsage::builder().input_tokens(5).time(1.5).build(),
            None,
            None,
        ));
        agg.append(&ChatResponse::new(
            "",
            vec![],
            ChatUsage::builder().input_tokens(3).time(0.0).build(),
            None,
            None,
        ));
        let resp = agg.response();
        assert_eq!(resp.usage.input_tokens, 5);
        assert_eq!(resp.usage.time, 1.5);
    }
}
