//! Mem0 long-term memory integration (`io.agentscope.core.memory.mem0`).

pub mod client;
pub mod dto;
pub mod memory;

pub use client::{
    Mem0Client, PLATFORM_MEMORIES_ENDPOINT, PLATFORM_SEARCH_ENDPOINT,
    SELF_HOSTED_MEMORIES_ENDPOINT, SELF_HOSTED_SEARCH_ENDPOINT,
};
pub use dto::{
    Mem0AddRequest, Mem0AddResponse, Mem0ApiType, Mem0Message, Mem0SearchRequest,
    Mem0SearchResponse, Mem0SearchResult,
};
pub use memory::{
    convert_messages, convert_to_mem0_message, join_search_results, role_to_mem0_role,
    Mem0LongTermMemory, Mem0LongTermMemoryConfig, COMPRESSED_HISTORY_MARKER, DEFAULT_SEARCH_TOP_K,
};
