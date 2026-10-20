//! Context material types.

use sha2::{Digest, Sha256};

/// Where a piece of material is placed in the final request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ContextItemPlacement {
    /// Stable instructions; trusted application configuration only.
    System,
    /// Current runtime state (task, plan mode, live business data).
    Runtime,
    /// Supporting documents the model may consult.
    Reference,
}

impl ContextItemPlacement {
    /// The wire name of this placement.
    pub fn as_str(&self) -> &'static str {
        match self {
            ContextItemPlacement::System => "SYSTEM",
            ContextItemPlacement::Runtime => "RUNTIME",
            ContextItemPlacement::Reference => "REFERENCE",
        }
    }
}

/// The purpose of one model preparation call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextPurpose {
    /// The main reasoning call: every material category participates.
    Reasoning,
    /// An auxiliary (e.g. summary) call: only System instructions participate.
    Summary,
}

impl ContextPurpose {
    /// The wire name of this purpose.
    pub fn as_str(&self) -> &'static str {
        match self {
            ContextPurpose::Reasoning => "REASONING",
            ContextPurpose::Summary => "SUMMARY",
        }
    }
}

/// Source material, prior to model rendering. Authority is determined by its provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextItem {
    pub kind: String,
    pub source_id: String,
    pub revision: String,
    pub placement: ContextItemPlacement,
    pub required: bool,
    pub priority: i32,
    pub content: String,
}

impl ContextItem {
    /// Creates a reference item whose revision is the content hash.
    pub fn new(
        kind: impl Into<String>,
        source_id: impl Into<String>,
        content: impl Into<String>,
    ) -> crate::Result<Self> {
        let kind = kind.into();
        let required = kind == "additional";
        Self::build(
            &kind,
            source_id,
            &content.into(),
            ContextItemPlacement::Reference,
            required,
            0,
        )
    }

    /// Creates a trusted instruction item: System placement, required, highest priority.
    pub fn instruction(
        kind: impl Into<String>,
        source_id: impl Into<String>,
        content: impl Into<String>,
    ) -> crate::Result<Self> {
        let kind = kind.into();
        Self::build(
            &kind,
            source_id,
            &content.into(),
            ContextItemPlacement::System,
            true,
            100,
        )
    }

    pub(crate) fn build(
        kind: &str,
        source_id: impl Into<String>,
        content: &str,
        placement: ContextItemPlacement,
        required: bool,
        priority: i32,
    ) -> crate::Result<Self> {
        if !is_valid_kind(kind) {
            return Err(crate::Error::Agent(format!("Invalid context kind: {kind}")));
        }
        let source_id = source_id.into();
        // A blank id would make the material impossible to name in an omission policy or
        // a manifest entry, so it is rejected at construction time.
        if source_id.trim().is_empty() {
            return Err(crate::Error::Agent("Block ID must not be blank".into()));
        }
        Ok(Self {
            kind: kind.to_string(),
            source_id,
            revision: content_hash(content),
            placement,
            required,
            priority,
            content: content.to_string(),
        })
    }

    /// Replaces the business revision (defaults to the content hash).
    pub fn with_revision(mut self, revision: impl Into<String>) -> crate::Result<Self> {
        let revision = revision.into();
        if revision.trim().is_empty() {
            return Err(crate::Error::Agent("Revision must not be blank".into()));
        }
        self.revision = revision;
        Ok(self)
    }

    /// Prevents budget eviction.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Sets the eviction priority within a material category (lower is evicted first).
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }
}

/// A dynamic context block, as supplied by an application source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBlock {
    item: ContextItem,
}

impl ContextBlock {
    /// Transient current business state (placed as runtime material).
    pub fn runtime(id: impl Into<String>, content: impl Into<String>) -> crate::Result<Self> {
        Ok(Self {
            item: ContextItem::build(
                "runtime",
                id,
                &content.into(),
                ContextItemPlacement::Runtime,
                false,
                0,
            )?,
        })
    }

    /// A supporting document (placed as reference material).
    pub fn reference(id: impl Into<String>, content: impl Into<String>) -> crate::Result<Self> {
        Ok(Self {
            item: ContextItem::build(
                "reference",
                id,
                &content.into(),
                ContextItemPlacement::Reference,
                false,
                0,
            )?,
        })
    }

    /// A business revision; not acceptance evidence.
    pub fn with_revision(mut self, revision: impl Into<String>) -> crate::Result<Self> {
        self.item = self.item.with_revision(revision)?;
        Ok(self)
    }

    /// Prevents budget eviction; cannot be combined with an `Omit` source.
    pub fn required(mut self) -> Self {
        self.item = self.item.required();
        self
    }

    /// Sets the eviction priority.
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.item = self.item.with_priority(priority);
        self
    }

    /// The underlying material.
    pub fn material(self) -> ContextItem {
        self.item
    }
}

/// Hex-encoded SHA-256 of a text.
pub fn content_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn is_valid_kind(kind: &str) -> bool {
    let mut chars = kind.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_blocks_default_to_the_reference_placement() {
        let item = ContextBlock::reference("format", "columns").unwrap().material();
        assert_eq!(item.placement, ContextItemPlacement::Reference);
        assert!(!item.required);
        assert_eq!(item.revision, content_hash("columns"));
    }

    #[test]
    fn runtime_blocks_can_never_become_system_instructions() {
        let item = ContextBlock::runtime("status", "Change 42").unwrap().material();
        assert_eq!(item.placement, ContextItemPlacement::Runtime);
        assert_ne!(item.placement, ContextItemPlacement::System);
    }

    #[test]
    fn instructions_are_required_system_material() {
        let item = ContextItem::instruction("policy", "instruction/x", "preserve API").unwrap();
        assert_eq!(item.placement, ContextItemPlacement::System);
        assert!(item.required);
        assert_eq!(item.priority, 100);
    }

    #[test]
    fn revision_priority_and_required_are_fluent() {
        let item = ContextBlock::reference("a", "b")
            .unwrap()
            .with_revision("v7")
            .unwrap()
            .required()
            .with_priority(-3)
            .material();
        assert_eq!(item.revision, "v7");
        assert!(item.required);
        assert_eq!(item.priority, -3);
        assert!(ContextBlock::reference("a", "b")
            .unwrap()
            .with_revision("   ")
            .is_err());
    }

    #[test]
    fn invalid_kinds_and_blank_ids_are_rejected() {
        assert!(ContextItem::new("Bad_Kind", "id", "x").is_err());
        assert!(ContextItem::new("1bad", "id", "x").is_err());
        assert!(ContextItem::new("good_kind", "id", "x").is_ok());
        assert!(ContextBlock::runtime("", "x").is_err());
    }

    #[test]
    fn content_hash_is_stable_sha256() {
        assert_eq!(
            content_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(content_hash("a"), content_hash("a"));
        assert_ne!(content_hash("a"), content_hash("b"));
    }
}
