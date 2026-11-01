pub mod block;
pub mod execution_details;
pub mod keys;
pub mod msg;
pub mod source;
pub mod usage;

pub use block::{
    AudioBlock, ContentBlock, DataBlock, HintBlock, ImageBlock, TextBlock, ThinkingBlock,
    ToolCallState, ToolResultBlock, ToolResultState, ToolUseBlock, VideoBlock,
};
pub use execution_details::{ToolExecutionDetails, ToolExecutionOutcome};
pub use keys::metadata_keys;
pub use msg::{current_timestamp, GenerateReason, Msg, MsgBuilder, MsgRole};
pub use source::{Base64Source, Source, URLSource};
pub use usage::ChatUsage;

/// JSON map used for message/block metadata and arbitrary tool inputs.
pub type JsonMap = serde_json::Map<String, serde_json::Value>;
