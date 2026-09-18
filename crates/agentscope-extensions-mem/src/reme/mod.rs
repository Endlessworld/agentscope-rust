//! ReMe long-term memory integration (`io.agentscope.core.memory.reme`).

pub mod client;
pub mod dto;
pub mod memory;

pub use client::{ReMeClient, RETRIEVE_ENDPOINT, SUMMARY_ENDPOINT};
pub use dto::{
    ReMeAddMetadata, ReMeAddRequest, ReMeAddResponse, ReMeMemoryItem, ReMeMessage,
    ReMeSearchMetadata, ReMeSearchRequest, ReMeSearchResponse, ReMeTrajectory, ReMeUpdateResult,
};
pub use memory::{
    convert_messages, convert_to_reme_message, is_recordable, render_search_response,
    role_to_reme_role, ReMeLongTermMemory, DEFAULT_SEARCH_TOP_K,
};
