//! Nacos-backed skill repository, mirroring
//! `io.agentscope.core.nacos.skill.NacosSkillRepository`.
//!
//! Implements the harness [`SkillRepository`] so Nacos skill packages slot into
//! the existing skill runtime without a parallel repository abstraction.

use std::collections::HashMap;
use std::sync::Arc;

use tracing::warn;

use agentscope_harness::agent::skill::model::AgentSkill;
use agentscope_harness::agent::skill::repository::SkillRepository;
use agentscope_harness::agent::skill::stager::SkillRepo;

use crate::service::{AiServiceError, NacosAiService};

pub mod frontmatter;

/// Repository kind identifier, mirroring Java `REPO_TYPE`.
pub const REPO_TYPE: &str = "nacos";
/// Location prefix, mirroring Java `LOCATION_PREFIX`.
pub const LOCATION_PREFIX: &str = "namespace:";

/// Property key for the skill version, mirroring `SKILL_VERSION_PATH`.
pub const SKILL_VERSION_PATH: &str = "agentscope.nacos.skill.version";
/// Property key for the skill label, mirroring `SKILL_LABEL_PATH`.
pub const SKILL_LABEL_PATH: &str = "agentscope.nacos.skill.label";
/// Environment variable for the skill version.
pub const ENV_SKILL_VERSION_PATH: &str = "AGENTSCOPE_NACOS_SKILL_VERSION";
/// Environment variable for the skill label.
pub const ENV_SKILL_LABEL_PATH: &str = "AGENTSCOPE_NACOS_SKILL_LABEL";

/// The default namespace when none is provided (mirrors Java's `public`).
pub const DEFAULT_NAMESPACE: &str = "public";

/// A [`SkillRepository`] backed by Nacos AI skill packages, mirroring
/// `NacosSkillRepository`.
///
/// Read operations download and parse skill ZIPs; write operations are no-ops
/// (the repository is always read-only). Enumeration requires `known_skill_names`
/// because Nacos exposes no "list all skills" API.
pub struct NacosSkillRepository {
    service: Arc<dyn NacosAiService>,
    namespace_id: String,
    source: String,
    location: String,
    skill_version: Option<String>,
    skill_label: Option<String>,
    known_skill_names: Vec<String>,
}

impl NacosSkillRepository {
    /// Creates a repository for `namespace_id` with no properties or known names.
    pub fn new(service: Arc<dyn NacosAiService>, namespace_id: impl Into<String>) -> Self {
        Self::with_known(service, namespace_id, None, Vec::new())
    }

    /// Creates a repository resolving version/label from `properties` and then
    /// environment variables. Mirrors the Java precedence.
    pub fn with_properties(
        service: Arc<dyn NacosAiService>,
        namespace_id: impl Into<String>,
        properties: Option<&HashMap<String, String>>,
    ) -> Self {
        Self::with_known(service, namespace_id, properties, Vec::new())
    }

    /// Creates a repository with an explicit set of enumerable skill names.
    pub fn with_known(
        service: Arc<dyn NacosAiService>,
        namespace_id: impl Into<String>,
        properties: Option<&HashMap<String, String>>,
        known_skill_names: Vec<String>,
    ) -> Self {
        let namespace_id = {
            let raw = namespace_id.into();
            if raw.trim().is_empty() {
                DEFAULT_NAMESPACE.to_string()
            } else {
                raw.trim().to_string()
            }
        };
        let source = format!("{REPO_TYPE}@{namespace_id}");
        let location = format!("{LOCATION_PREFIX}{namespace_id}");

        let skill_version = first_non_blank([
            properties.and_then(|p| p.get(SKILL_VERSION_PATH).cloned()),
            std::env::var(SKILL_VERSION_PATH).ok(),
            std::env::var(ENV_SKILL_VERSION_PATH).ok(),
        ]);
        let skill_label = first_non_blank([
            properties.and_then(|p| p.get(SKILL_LABEL_PATH).cloned()),
            std::env::var(SKILL_LABEL_PATH).ok(),
            std::env::var(ENV_SKILL_LABEL_PATH).ok(),
        ]);

        Self {
            service,
            namespace_id,
            source,
            location,
            skill_version,
            skill_label,
            known_skill_names,
        }
    }

    /// The namespace this repository reads from.
    pub fn namespace_id(&self) -> &str {
        &self.namespace_id
    }

    /// The location string (`namespace:<id>`), mirroring `getRepositoryInfo().location`.
    pub fn location(&self) -> &str {
        &self.location
    }

    /// The resolved skill version, if any.
    pub fn skill_version(&self) -> Option<&str> {
        self.skill_version.as_deref()
    }

    /// The resolved skill label, if any.
    pub fn skill_label(&self) -> Option<&str> {
        self.skill_label.as_deref()
    }

    /// Downloads a skill ZIP bytes using the version/label precedence: version
    /// wins when both resolve. Mirrors `downloadSkillZipBytes`.
    async fn download_skill_zip_bytes(&self, skill_name: &str) -> Result<Vec<u8>, AiServiceError> {
        if let Some(version) = self.skill_version.as_deref() {
            return self
                .service
                .download_skill_zip_by_version(skill_name, version)
                .await;
        }
        if let Some(label) = self.skill_label.as_deref() {
            return self
                .service
                .download_skill_zip_by_label(skill_name, label)
                .await;
        }
        self.service.download_skill_zip(skill_name).await
    }

    /// Loads a skill from Nacos, mirroring `getSkill(String)`. Returns `Err` on
    /// a missing or malformed skill.
    pub async fn get_skill(&self, name: &str) -> Result<AgentSkill, String> {
        if name.trim().is_empty() {
            return Err("Skill name cannot be null or empty".to_string());
        }
        let zip_bytes = self
            .download_skill_zip_bytes(name.trim())
            .await
            .map_err(|e| {
                if e.is_not_found() {
                    format!("Skill not found: {name}")
                } else {
                    format!("Failed to load skill from Nacos: {name}: {e}")
                }
            })?;
        if zip_bytes.is_empty() {
            return Err(format!("Skill not found: {name}"));
        }
        let adapted = frontmatter::adapt_nacos_skill_zip(&zip_bytes);
        frontmatter::create_from_zip(&adapted, &self.source, name.trim())
    }

    /// Whether a skill exists in Nacos, mirroring `skillExists(String)`.
    pub async fn skill_exists(&self, skill_name: &str) -> bool {
        if skill_name.trim().is_empty() {
            return false;
        }
        match self.download_skill_zip_bytes(skill_name.trim()).await {
            Ok(bytes) => !bytes.is_empty(),
            Err(e) if e.is_not_found() => false,
            Err(_) => false,
        }
    }
}

impl SkillRepository for NacosSkillRepository {
    fn repo_id(&self) -> String {
        format!("nacos:{}", self.namespace_id)
    }

    fn source(&self) -> String {
        self.source.clone()
    }

    fn is_writeable(&self) -> bool {
        false
    }

    fn repo_kind(&self) -> SkillRepo {
        SkillRepo::External {
            source: self.source.clone(),
        }
    }

    /// Loads every skill named in `known_skill_names`, fetching each in turn
    /// (one network call per skill). Empty when no names were configured.
    fn load(&self) -> Vec<AgentSkill> {
        if self.known_skill_names.is_empty() {
            warn!(
                "NacosSkillRepository: no knownSkillNames configured, returning empty list. \
                 Use with_known(...) to enable enumeration."
            );
            return Vec::new();
        }
        // The trait is synchronous; block on each download by driving the futures
        // on a detached runtime. Mirrors Java's blocking `getAllSkills`.
        let mut result = Vec::with_capacity(self.known_skill_names.len());
        for name in &self.known_skill_names {
            match block_on(self.get_skill(name)) {
                Ok(skill) => result.push(skill),
                Err(e) => warn!(skill = %name, error = %e, "Failed to load skill, skipping"),
            }
        }
        result
    }
}

/// Drives `fut` to completion, reusing the current Tokio runtime when present
/// and otherwise creating a temporary current-thread runtime.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build a temporary Tokio runtime")
            .block_on(fut),
    }
}

/// Returns the first non-null, non-blank trimmed candidate.
fn first_non_blank<const N: usize>(candidates: [Option<String>; N]) -> Option<String> {
    candidates.into_iter().find_map(|c| {
        let t = c?;
        if t.trim().is_empty() {
            None
        } else {
            Some(t.trim().to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::InMemoryNacosAiService;
    use agentscope_harness::agent::skill::stager::SkillRepo;
    use std::io::Write as _;

    fn make_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        use zip::write::FileOptions;
        use zip::{CompressionMethod, ZipWriter};
        let mut buf = Vec::new();
        {
            let mut w = ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts = FileOptions::default().compression_method(CompressionMethod::Deflated);
            for (name, content) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(content.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    fn service_with_skill(name: &str) -> Arc<InMemoryNacosAiService> {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let md = format!("---\nname: {name}\ndescription: The {name} skill\n---\nBody");
        let zip = make_zip(&[(format!("{name}/SKILL.md").as_str(), md.as_str())]);
        svc.set_skill_zip(name, zip);
        svc
    }

    #[test]
    fn repo_metadata_matches_java() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let repo = NacosSkillRepository::new(svc, "");
        assert_eq!(repo.namespace_id(), DEFAULT_NAMESPACE);
        assert_eq!(repo.source(), "nacos@public");
        assert_eq!(repo.location(), "namespace:public");
        assert!(!repo.is_writeable());
        assert_eq!(
            repo.repo_kind(),
            SkillRepo::External {
                source: "nacos@public".to_string()
            }
        );
    }

    #[tokio::test]
    async fn get_skill_downloads_and_parses() {
        let svc = service_with_skill("demo");
        let repo = NacosSkillRepository::new(svc, "public");
        let skill = repo.get_skill("demo").await.unwrap();
        assert_eq!(skill.name(), "demo");
        assert_eq!(skill.description(), "The demo skill");
        assert_eq!(skill.source(), "nacos@public");
    }

    #[tokio::test]
    async fn skill_exists_and_missing() {
        let svc = service_with_skill("demo");
        let repo = NacosSkillRepository::new(svc, "public");
        assert!(repo.skill_exists("demo").await);
        assert!(!repo.skill_exists("nope").await);
    }

    #[tokio::test]
    async fn get_missing_skill_is_error() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let repo = NacosSkillRepository::new(svc, "public");
        let err = repo.get_skill("nope").await.unwrap_err();
        assert!(err.contains("Skill not found"));
    }

    #[test]
    fn load_returns_known_skills() {
        let svc = service_with_skill("demo");
        let repo = NacosSkillRepository::with_known(
            svc,
            "public",
            None,
            vec!["demo".to_string(), "missing".to_string()],
        );
        let skills = repo.load();
        // "missing" is skipped; "demo" loads despite no external runtime.
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name(), "demo");
    }

    #[test]
    fn load_without_known_names_is_empty() {
        let svc = service_with_skill("demo");
        let repo = NacosSkillRepository::new(svc, "public");
        assert!(repo.load().is_empty());
    }

    #[test]
    fn version_and_label_precedence_from_properties() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let mut props = HashMap::new();
        props.insert(SKILL_VERSION_PATH.to_string(), "1.2.3".to_string());
        props.insert(SKILL_LABEL_PATH.to_string(), "prod".to_string());
        let repo = NacosSkillRepository::with_properties(svc, "public", Some(&props));
        assert_eq!(repo.skill_version(), Some("1.2.3"));
        assert_eq!(repo.skill_label(), Some("prod"));
    }

    #[test]
    fn version_takes_precedence_over_label_on_download() {
        // Java: `downloadSkillZipBytes` prefers version; the label is not used.
        let svc = Arc::new(InMemoryNacosAiService::new());
        let mut props = HashMap::new();
        props.insert(SKILL_VERSION_PATH.to_string(), "2.0".to_string());
        props.insert(SKILL_LABEL_PATH.to_string(), "prod".to_string());
        let repo = NacosSkillRepository::with_properties(svc, "public", Some(&props));
        assert_eq!(repo.skill_version(), Some("2.0"));
        assert_eq!(repo.skill_label(), Some("prod"));
    }

    #[test]
    fn blank_namespace_falls_back_to_public() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let repo = NacosSkillRepository::new(svc, "   ");
        assert_eq!(repo.namespace_id(), DEFAULT_NAMESPACE);
        assert_eq!(repo.namespace_id(), "public");
    }

    #[test]
    fn write_operations_are_rejected() {
        // The harness defaults are inherited: a read-only repository refuses writes.
        let svc = Arc::new(InMemoryNacosAiService::new());
        let repo = NacosSkillRepository::new(svc, "public");
        assert!(!repo.is_writeable());
        assert!(repo.skill_dir("demo").is_none());

        let skill = agentscope_harness::agent::skill::model::AgentSkill::builder()
            .name("demo")
            .source("nacos@public")
            .description("d")
            .build();
        assert!(repo.write_skill(&skill).is_err());
        assert!(repo.delete_skill("demo").is_err());
    }

    #[tokio::test]
    async fn blank_skill_name_is_rejected() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let repo = NacosSkillRepository::new(svc, "public");
        assert!(repo.get_skill("   ").await.is_err());
        assert!(!repo.skill_exists("   ").await);
    }
}
