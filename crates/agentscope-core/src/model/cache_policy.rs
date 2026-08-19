//! Controls whether the model registry caches created models.

/// Cache policy for model creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// Preserve registry defaults (legacy behavior).
    Default,
    /// Never cache the created model.
    Disabled,
    /// Cache the created model using an explicit or safely derived identity.
    Enabled,
}
