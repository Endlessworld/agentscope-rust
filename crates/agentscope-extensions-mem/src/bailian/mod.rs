//! Bailian Long Term Memory integration (`io.agentscope.core.memory.bailian`).

pub mod client;
pub mod dto;
pub mod memory;

pub use client::{
    BailianMemoryClient, ADD_MEMORY_ENDPOINT, DEFAULT_BASE_URL, SEARCH_MEMORY_ENDPOINT,
};
pub use dto::{
    BailianAddRequest, BailianAddResponse, BailianMemoryNode, BailianMessage,
    BailianSearchRequest, BailianSearchResponse,
};
pub use memory::{
    convert_messages, convert_to_bailian_message, is_recordable, join_memory_nodes,
    role_to_bailian_role, BailianLongTermMemory, BailianLongTermMemoryConfig, DEFAULT_MIN_SCORE,
    DEFAULT_TOP_K,
};
