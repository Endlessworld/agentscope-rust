//! Usage (token accounting) helpers shared by every stream decoder.
//!
//! Upstreams report usage inconsistently: Anthropic splits it across
//! `message_start` (prompt) and `message_delta` (output), while several
//! OpenAI-compatible gateways only send a single usage frame — or none at all.
//! Naively overwriting the accumulated value loses whatever a later frame does
//! not repeat, so all decoders use [`merge_non_zero`] instead (mirrors new-api's
//! `MergeUsageNonZero`).

use agentscope_core::message::ChatUsage;

/// Merges `incoming` into `current`, letting only **non-zero** incoming fields
/// win: a later frame that reports `0` never clears a positive value already
/// seen.
///
/// `time` is treated as a monotonic duration and kept at its maximum.
pub fn merge_non_zero(current: &ChatUsage, incoming: &ChatUsage) -> ChatUsage {
    ChatUsage {
        input_tokens: pick(current.input_tokens, incoming.input_tokens),
        output_tokens: pick(current.output_tokens, incoming.output_tokens),
        cached_tokens: pick(current.cached_tokens, incoming.cached_tokens),
        time: if incoming.time > current.time {
            incoming.time
        } else {
            current.time
        },
    }
}

/// In-place variant of [`merge_non_zero`].
pub fn merge_into(current: &mut ChatUsage, incoming: &ChatUsage) {
    *current = merge_non_zero(current, incoming);
}

fn pick(current: u64, incoming: u64) -> u64 {
    if incoming > 0 {
        incoming
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_non_zero_fields_override() {
        let mut acc = ChatUsage { input_tokens: 10, output_tokens: 0, cached_tokens: 3, time: 0.0 };
        merge_into(
            &mut acc,
            &ChatUsage { input_tokens: 0, output_tokens: 20, cached_tokens: 0, time: 0.0 },
        );
        assert_eq!(acc.input_tokens, 10, "zero must not clear a previous value");
        assert_eq!(acc.output_tokens, 20);
        assert_eq!(acc.cached_tokens, 3);
    }

    #[test]
    fn zero_frames_are_no_ops() {
        let acc = ChatUsage { input_tokens: 5, output_tokens: 6, cached_tokens: 1, time: 2.0 };
        let merged = merge_non_zero(&acc, &ChatUsage::default());
        assert_eq!(merged, acc);
    }

    #[test]
    fn time_keeps_maximum() {
        let acc = ChatUsage { time: 5.0, ..Default::default() };
        let merged = merge_non_zero(&acc, &ChatUsage { time: 1.5, ..Default::default() });
        assert_eq!(merged.time, 5.0);
    }
}
