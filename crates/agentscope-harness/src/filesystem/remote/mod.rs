//! Store-backed filesystem, mirroring
//! `io.agentscope.harness.agent.filesystem.remote.RemoteFilesystem`.
//!
//! Files are organized via namespaces and persist across threads/sessions. The
//! namespace can be static (fixed at construction time) or dynamic (resolved on
//! every operation via a [`NamespaceFactory`]).

pub mod store;

use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};

use super::local::{build_glob_matcher, PatternMatcher};
use super::util;
use super::{validate_path, FileData, FileInfo, Filesystem, GrepMatch};
use store::{BaseStore, DefaultNamespace, NamespaceFactory, StaticNamespace, StoreItem, StoreValue};

/// Maximum CAS retry attempts for `edit` under concurrent writers.
const EDIT_MAX_RETRIES: usize = 5;

/// Store page size for full scans.
const SCAN_PAGE_SIZE: usize = 100;

/// [`Filesystem`] backed by a [`BaseStore`] (persistent, cross-thread).
pub struct RemoteFilesystem {
    store: Arc<dyn BaseStore>,
    namespace_factory: Arc<dyn NamespaceFactory>,
}

impl RemoteFilesystem {
    /// Creates a `RemoteFilesystem` whose namespace is resolved per operation.
    pub fn new(
        store: Arc<dyn BaseStore>,
        namespace_factory: Arc<dyn NamespaceFactory>,
    ) -> Self {
        Self { store, namespace_factory }
    }

    /// Creates a `RemoteFilesystem` with a fixed namespace tuple.
    pub fn with_namespace(store: Arc<dyn BaseStore>, namespace: Vec<String>) -> Self {
        Self::new(store, Arc::new(StaticNamespace(namespace)))
    }

    /// Creates a `RemoteFilesystem` with the default `["filesystem"]` namespace.
    pub fn with_default_namespace(store: Arc<dyn BaseStore>) -> Self {
        Self::new(store, Arc::new(DefaultNamespace))
    }

    /// Returns the store backing this filesystem.
    pub fn store(&self) -> &Arc<dyn BaseStore> {
        &self.store
    }

    fn namespace(&self, ctx: &RuntimeContext) -> Result<Vec<String>> {
        let ns = self.namespace_factory.get_namespace(ctx);
        if ns.is_empty() {
            return Err(Error::Tool(
                "NamespaceFactory returned an empty namespace".to_string(),
            ));
        }
        Ok(ns)
    }

    /// Resolves an item, tolerating the presence/absence of a leading `/`.
    fn get_item(&self, ctx: &RuntimeContext, path: &str) -> Result<Option<StoreItem>> {
        let ns = self.namespace(ctx)?;
        for candidate in [path.to_string(), normalize_path(path)] {
            if let Some(item) = self.store.get(&ns, &candidate) {
                return Ok(Some(item));
            }
        }
        Ok(None)
    }

    /// Full remote scan, normalizing every key to a leading `/`.
    fn search_all_items(&self, ctx: &RuntimeContext) -> Result<Vec<StoreItem>> {
        let ns = self.namespace(ctx)?;
        let mut all = Vec::new();
        let mut offset = 0usize;
        loop {
            let page = self.store.search(&ns, SCAN_PAGE_SIZE, offset);
            if page.is_empty() {
                break;
            }
            for item in &page {
                all.push(normalize_item_key(item.clone()));
            }
            if page.len() < SCAN_PAGE_SIZE {
                break;
            }
            offset += SCAN_PAGE_SIZE;
        }
        Ok(all)
    }

    fn ctx_namespace(&self, ctx: &RuntimeContext) -> Result<Vec<String>> {
        self.namespace(ctx)
    }
}

fn normalize_item_key(item: StoreItem) -> StoreItem {
    if !item.key.starts_with('/') {
        StoreItem::new(format!("/{}", item.key), item.value, item.version)
    } else {
        item
    }
}

fn convert_item_to_file_data(item: &StoreItem) -> Option<FileData> {
    let content_obj = item.value.get("content")?;
    let content = match content_obj {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(list) => {
            let mut parts = Vec::with_capacity(list.len());
            for v in list {
                parts.push(match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                });
            }
            parts.join("\n")
        }
        _ => return None,
    };
    let encoding = item
        .value
        .get("encoding")
        .and_then(|v| v.as_str())
        .unwrap_or("utf-8")
        .to_string();
    Some(FileData { content, encoding })
}

fn file_data_to_store_value(fd: &FileData) -> StoreValue {
    let mut result = StoreValue::new();
    result.insert("content".to_string(), serde_json::Value::String(fd.content.clone()));
    result.insert("encoding".to_string(), serde_json::Value::String(fd.encoding.clone()));
    result
}

fn normalize_path(path: &str) -> String {
    if path.trim().is_empty() {
        return "/".to_string();
    }
    let mut normalized = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if normalized != "/" && normalized.ends_with('/') {
        normalized.pop();
    }
    normalized
}

fn matches_path_prefix(key: &str, normalized_path: &str) -> bool {
    if normalized_path == "/" {
        return key.starts_with('/');
    }
    key == normalized_path || key.starts_with(&format!("{normalized_path}/"))
}

/// Computes the path of `key` relative to `normalized_path` (both leading-`/`).
fn relative_to(key: &str, normalized_path: &str) -> String {
    if normalized_path == "/" {
        key.strip_prefix('/').unwrap_or(key).to_string()
    } else if let Some(rest) = key.strip_prefix(&format!("{normalized_path}/")) {
        rest.to_string()
    } else {
        key.to_string()
    }
}

#[async_trait]
impl Filesystem for RemoteFilesystem {
    async fn ls(&self, ctx: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>> {
        let normalized_path = {
            let n = normalize_path(path);
            if n.ends_with('/') {
                n
            } else {
                format!("{n}/")
            }
        };
        let items = self.search_all_items(ctx)?;
        let mut infos = Vec::new();
        let mut subdirs: Vec<String> = Vec::new();
        for item in items {
            if !item.key.starts_with(&normalized_path) {
                continue;
            }
            let relative = &item.key[normalized_path.len()..];
            if let Some(idx) = relative.find('/') {
                let subdir = format!("{normalized_path}{}/", &relative[..idx]);
                if !subdirs.contains(&subdir) {
                    subdirs.push(subdir);
                }
                continue;
            }
            if let Some(fd) = convert_item_to_file_data(&item) {
                infos.push(FileInfo::file(item.key, fd.content.chars().count() as u64));
            }
        }
        for subdir in subdirs {
            infos.push(FileInfo::dir(subdir));
        }
        infos.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(infos)
    }

    async fn read(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<FileData> {
        let Some(item) = self.get_item(ctx, path)? else {
            return Err(Error::Tool(format!("File '{path}' not found")));
        };
        let Some(file_data) = convert_item_to_file_data(&item) else {
            return Err(Error::Tool(format!("Invalid file data for '{path}'")));
        };
        if util::get_file_type(path) != "text" {
            return Ok(file_data);
        }
        if file_data.content.trim().is_empty() {
            return Ok(file_data);
        }
        let lines: Vec<&str> = file_data.content.split('\n').collect();
        let start = offset;
        let end = if limit > 0 {
            (start + limit).min(lines.len())
        } else {
            lines.len()
        };
        if start >= lines.len() {
            return Err(Error::Tool(format!(
                "Line offset {offset} exceeds file length ({} lines)",
                lines.len()
            )));
        }
        Ok(FileData {
            content: lines[start..end].join("\n"),
            encoding: file_data.encoding,
        })
    }

    async fn write(&self, ctx: &RuntimeContext, path: &str, content: &str) -> Result<()> {
        let ns = self.ctx_namespace(ctx)?;
        let fd = FileData::text(content);
        // CAS create-if-absent: atomic under concurrent writers across nodes.
        let written =
            self.store
                .put_if_version(&ns, path, file_data_to_store_value(&fd), 0);
        if !written {
            return Err(Error::Tool(format!(
                "Cannot write to {path} because it already exists. \
                 Read and then make an edit, or write to a new path."
            )));
        }
        Ok(())
    }

    async fn edit(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize> {
        let ns = self.ctx_namespace(ctx)?;
        for _ in 0..EDIT_MAX_RETRIES {
            let Some(item) = self.get_item(ctx, path)? else {
                return Err(Error::Tool(format!("Error: File '{path}' not found")));
            };
            let Some(file_data) = convert_item_to_file_data(&item) else {
                return Err(Error::Tool("Error: Invalid file data".to_string()));
            };
            let (new_content, occurrences) =
                util::perform_string_replacement(&file_data.content, old, new, replace_all)
                    .map_err(Error::Tool)?;
            let updated = FileData { content: new_content, encoding: file_data.encoding };
            let stored_key = item.key.clone();
            if self.store.put_if_version(
                &ns,
                &stored_key,
                file_data_to_store_value(&updated),
                item.version,
            ) {
                return Ok(occurrences);
            }
            // Version mismatch — another writer raced us. Re-read and retry.
        }
        Err(Error::Tool(format!(
            "Edit conflict on '{path}' after {EDIT_MAX_RETRIES} retries. \
             Another writer is concurrently modifying this file."
        )))
    }

    async fn grep(
        &self,
        ctx: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>> {
        let normalized_path = normalize_path(path.unwrap_or("/"));
        let glob_matcher = glob
            .filter(|g| !g.trim().is_empty())
            .and_then(build_glob_matcher);
        let matcher = PatternMatcher::new(pattern);

        let items = self.search_all_items(ctx)?;
        let mut matches = Vec::new();
        for item in items {
            let key = item.key.clone();
            if !matches_path_prefix(&key, &normalized_path) {
                continue;
            }
            if let Some(gm) = &glob_matcher {
                let file_name = match key.rfind('/') {
                    Some(i) => &key[i + 1..],
                    None => key.as_str(),
                };
                if !gm.is_match(std::path::Path::new(file_name)) {
                    continue;
                }
            }
            let Some(fd) = convert_item_to_file_data(&item) else {
                continue;
            };
            let lines: Vec<&str> = fd.content.split('\n').collect();
            for (i, line) in lines.iter().enumerate() {
                if matcher.is_match(line) {
                    matches.push(GrepMatch {
                        path: key.clone(),
                        line: i + 1,
                        text: (*line).to_string(),
                    });
                }
            }
        }
        Ok(matches)
    }

    async fn glob(&self, ctx: &RuntimeContext, pattern: &str, path: &str) -> Result<Vec<FileInfo>> {
        let normalized_path = normalize_path(path);
        let effective_pattern = pattern.strip_prefix('/').unwrap_or(pattern);
        let recursive_pattern = if effective_pattern.starts_with("**") {
            effective_pattern.to_string()
        } else {
            format!("**/{effective_pattern}")
        };
        let matcher = build_glob_matcher(&recursive_pattern);
        let direct_pattern = if let Some(rest) = effective_pattern.strip_prefix("**/") {
            rest.to_string()
        } else if effective_pattern == "**" {
            "*".to_string()
        } else {
            effective_pattern.to_string()
        };
        let direct_matcher = build_glob_matcher(&direct_pattern);

        let items = self.search_all_items(ctx)?;
        let mut results = Vec::new();
        for item in items {
            let key = item.key.clone();
            if !matches_path_prefix(&key, &normalized_path) {
                continue;
            }
            let relative = relative_to(&key, &normalized_path);
            let rel_path = std::path::Path::new(&relative);
            let matched = matcher.as_ref().is_some_and(|m| m.is_match(rel_path))
                || direct_matcher.as_ref().is_some_and(|m| m.is_match(rel_path));
            if matched {
                let size = convert_item_to_file_data(&item)
                    .map(|fd| fd.content.chars().count() as u64)
                    .unwrap_or(0);
                results.push(FileInfo::file(key, size));
            }
        }
        results.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(results)
    }

    async fn delete(&self, ctx: &RuntimeContext, path: &str) -> Result<()> {
        validate_path(path)?;
        let ns = self.ctx_namespace(ctx)?;
        let normalized_path = normalize_path(path);
        for item in self.search_all_items(ctx)? {
            if item.key == normalized_path
                || item.key.starts_with(&format!("{normalized_path}/"))
                || item.key == path
            {
                self.store.delete(&ns, &item.key);
            }
        }
        // Idempotent — not found is still success.
        Ok(())
    }

    async fn move_path(&self, ctx: &RuntimeContext, from: &str, to: &str) -> Result<()> {
        validate_path(from)?;
        validate_path(to)?;
        let ns = self.ctx_namespace(ctx)?;
        let norm_from = normalize_path(from);
        let norm_to = normalize_path(to);
        let mut found = false;
        for item in self.search_all_items(ctx)? {
            let key = item.key.clone();
            if key == norm_from || key.starts_with(&format!("{norm_from}/")) {
                let new_key = format!("{norm_to}{}", &key[norm_from.len()..]);
                self.store.put(&ns, &new_key, item.value.clone());
                self.store.delete(&ns, &key);
                found = true;
            }
        }
        if !found {
            return Err(Error::Tool(format!("Source does not exist: {from}")));
        }
        Ok(())
    }

    async fn exists(&self, ctx: &RuntimeContext, path: &str) -> Result<bool> {
        if path.trim().is_empty() {
            return Ok(false);
        }
        let normalized = normalize_path(path);
        if self.get_item(ctx, path)?.is_some() {
            return Ok(true);
        }
        for item in self.search_all_items(ctx)? {
            if item.key.starts_with(&format!("{normalized}/")) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use store::InMemoryStore;

    fn fs() -> RemoteFilesystem {
        RemoteFilesystem::with_default_namespace(Arc::new(InMemoryStore::new()))
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let fs = fs();
        let ctx = RuntimeContext::new();
        fs.write(&ctx, "/notes/a.md", "line1\nline2\nline3").await.unwrap();
        let data = fs.read(&ctx, "/notes/a.md", 0, 0).await.unwrap();
        assert_eq!(data.content, "line1\nline2\nline3");
        // offset/limit slice
        let sliced = fs.read(&ctx, "/notes/a.md", 1, 1).await.unwrap();
        assert_eq!(sliced.content, "line2");
    }

    #[tokio::test]
    async fn write_is_create_if_absent() {
        let fs = fs();
        let ctx = RuntimeContext::new();
        fs.write(&ctx, "/a.md", "one").await.unwrap();
        assert!(fs.write(&ctx, "/a.md", "two").await.is_err());
    }

    #[tokio::test]
    async fn edit_uses_cas_and_counts_occurrences() {
        let fs = fs();
        let ctx = RuntimeContext::new();
        fs.write(&ctx, "/a.md", "foo bar foo").await.unwrap();
        let n = fs.edit(&ctx, "/a.md", "foo", "baz", true).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(fs.read(&ctx, "/a.md", 0, 0).await.unwrap().content, "baz bar baz");
    }

    #[tokio::test]
    async fn ls_reports_files_and_subdirs() {
        let fs = fs();
        let ctx = RuntimeContext::new();
        fs.write(&ctx, "/root/a.md", "a").await.unwrap();
        fs.write(&ctx, "/root/sub/b.md", "b").await.unwrap();
        let entries = fs.ls(&ctx, "/root").await.unwrap();
        let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
        assert!(paths.contains(&"/root/a.md".to_string()));
        assert!(paths.contains(&"/root/sub/".to_string()));
        assert!(entries.iter().any(|e| e.is_directory && e.path == "/root/sub/"));
    }

    #[tokio::test]
    async fn glob_matches_nested_and_root_level() {
        let fs = fs();
        let ctx = RuntimeContext::new();
        fs.write(&ctx, "/root/a.md", "a").await.unwrap();
        fs.write(&ctx, "/root/deep/b.md", "b").await.unwrap();
        fs.write(&ctx, "/root/c.txt", "c").await.unwrap();
        let mut matches: Vec<String> = fs
            .glob(&ctx, "**/*.md", "/root")
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.path)
            .collect();
        matches.sort();
        assert_eq!(matches, vec!["/root/a.md".to_string(), "/root/deep/b.md".to_string()]);
    }

    #[tokio::test]
    async fn grep_finds_matching_lines() {
        let fs = fs();
        let ctx = RuntimeContext::new();
        fs.write(&ctx, "/root/a.md", "alpha\nbeta\nalpha2").await.unwrap();
        let matches = fs.grep(&ctx, "alpha", Some("/root"), None).await.unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line, 1);
        assert_eq!(matches[1].line, 3);
    }

    #[tokio::test]
    async fn exists_detects_files_and_directories() {
        let fs = fs();
        let ctx = RuntimeContext::new();
        fs.write(&ctx, "/root/deep/a.md", "a").await.unwrap();
        assert!(fs.exists(&ctx, "/root/deep/a.md").await.unwrap());
        assert!(fs.exists(&ctx, "/root/deep").await.unwrap());
        assert!(!fs.exists(&ctx, "/root/nope").await.unwrap());
    }

    #[tokio::test]
    async fn delete_removes_subtree() {
        let fs = fs();
        let ctx = RuntimeContext::new();
        fs.write(&ctx, "/root/a.md", "a").await.unwrap();
        fs.write(&ctx, "/root/sub/b.md", "b").await.unwrap();
        fs.delete(&ctx, "/root").await.unwrap();
        assert!(!fs.exists(&ctx, "/root/a.md").await.unwrap());
        assert!(!fs.exists(&ctx, "/root/sub/b.md").await.unwrap());
    }

    #[tokio::test]
    async fn move_renames_subtree() {
        let fs = fs();
        let ctx = RuntimeContext::new();
        fs.write(&ctx, "/old/a.md", "a").await.unwrap();
        fs.write(&ctx, "/old/sub/b.md", "b").await.unwrap();
        fs.move_path(&ctx, "/old", "/new").await.unwrap();
        assert!(fs.exists(&ctx, "/new/a.md").await.unwrap());
        assert!(fs.exists(&ctx, "/new/sub/b.md").await.unwrap());
        assert!(!fs.exists(&ctx, "/old/a.md").await.unwrap());
    }

    #[tokio::test]
    async fn move_missing_source_errors() {
        let fs = fs();
        let ctx = RuntimeContext::new();
        assert!(fs.move_path(&ctx, "/nope", "/dst").await.is_err());
    }

    #[tokio::test]
    async fn namespaces_are_isolated_by_runtime_context() {
        use std::sync::Arc as StdArc;
        let store: StdArc<dyn BaseStore> = StdArc::new(InMemoryStore::new());
        struct UserNs;
        impl NamespaceFactory for UserNs {
            fn get_namespace(&self, ctx: &RuntimeContext) -> Vec<String> {
                vec![
                    "users".to_string(),
                    ctx.user_id().unwrap_or("anon").to_string(),
                    "files".to_string(),
                ]
            }
        }
        let fs = RemoteFilesystem::new(store, StdArc::new(UserNs));
        let ctx_a = RuntimeContext::new().with_user_id("alice");
        let ctx_b = RuntimeContext::new().with_user_id("bob");
        fs.write(&ctx_a, "/a.md", "alice-data").await.unwrap();
        fs.write(&ctx_b, "/a.md", "bob-data").await.unwrap();
        assert_eq!(fs.read(&ctx_a, "/a.md", 0, 0).await.unwrap().content, "alice-data");
        assert_eq!(fs.read(&ctx_b, "/a.md", 0, 0).await.unwrap().content, "bob-data");
    }
}
