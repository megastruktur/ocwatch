use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::ipc::RecentDirEntry;

const MAX_RECENT_DIRS: usize = 100;

#[derive(Default)]
pub struct RecentDirStore {
    entries: Vec<RecentDirEntry>,
}

impl RecentDirStore {
    pub fn load(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read recent dir cache: {}", path.display()))?;
        let entries = serde_json::from_str::<Vec<RecentDirEntry>>(&raw)
            .with_context(|| format!("Failed to parse recent dir cache: {}", path.display()))?;

        let mut store = Self { entries };
        store.normalize();
        Ok(store)
    }

    pub fn save(&self, path: &PathBuf) -> Result<()> {
        let parent = path.parent().ok_or_else(|| {
            anyhow::anyhow!("Recent dir cache path has no parent: {}", path.display())
        })?;
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create recent dir cache dir: {}",
                parent.display()
            )
        })?;

        let tmp_path = path.with_extension("json.tmp");
        let payload = serde_json::to_vec_pretty(&self.entries)
            .context("Failed to serialize recent dir cache")?;
        std::fs::write(&tmp_path, payload)
            .with_context(|| format!("Failed to write recent dir cache: {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "Failed to atomically replace recent dir cache: {}",
                path.display()
            )
        })?;
        Ok(())
    }

    pub fn upsert(&mut self, entry: RecentDirEntry) {
        self.entries.retain(|existing| {
            !(existing.host == entry.host && existing.directory == entry.directory)
        });
        self.entries.push(entry);
        self.normalize();
    }

    pub fn upsert_many<I>(&mut self, entries: I)
    where
        I: IntoIterator<Item = RecentDirEntry>,
    {
        for entry in entries {
            self.upsert(entry);
        }
    }

    pub fn entries(&self, limit: usize) -> Vec<RecentDirEntry> {
        self.entries.iter().take(limit).cloned().collect()
    }

    pub fn has_host_entries(&self, host: &str) -> bool {
        self.entries.iter().any(|entry| entry.host == host)
    }

    fn normalize(&mut self) {
        self.entries.sort_by(|left, right| {
            right
                .last_seen_unix_ms
                .cmp(&left.last_seen_unix_ms)
                .then_with(|| left.host.cmp(&right.host))
                .then_with(|| left.directory.cmp(&right.directory))
        });
        self.entries.truncate(MAX_RECENT_DIRS);
    }
}
