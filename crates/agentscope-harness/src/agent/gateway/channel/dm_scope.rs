//! DM session scoping, mirroring `DmScope`.

/// Controls how DM (direct) session keys are scoped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DmScope {
    /// All DMs for a given agent share a single session.
    #[default]
    Main,
    /// One session per peer id.
    PerPeer,
    /// Like `PerPeer` but the channel name is included in the key.
    PerChannelPeer,
    /// Extends `PerChannelPeer` with the account id (multi-bot deployments).
    PerAccountChannelPeer,
}

impl DmScope {
    /// Default scope used when none is configured.
    pub fn default_scope() -> Self {
        Self::Main
    }
}
