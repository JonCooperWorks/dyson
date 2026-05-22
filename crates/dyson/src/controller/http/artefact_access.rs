use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::Result;
use crate::message::ArtefactKind;
use crate::tool::artefacts::{ArtefactReader, ArtefactRecord, ArtefactSummary, safe_store_id};

use super::stores::{ArtefactEntry, ArtefactStore};

#[derive(Clone)]
pub(crate) struct HttpArtefactReader {
    store: Arc<Mutex<ArtefactStore>>,
    data_dir: Option<PathBuf>,
}

impl HttpArtefactReader {
    pub(crate) const fn new(store: Arc<Mutex<ArtefactStore>>, data_dir: Option<PathBuf>) -> Self {
        Self { store, data_dir }
    }
}

impl ArtefactReader for HttpArtefactReader {
    fn list(&self, chat_id: &str, limit: usize) -> Result<Vec<ArtefactSummary>> {
        if !safe_store_id(chat_id) {
            return Ok(Vec::new());
        }

        let mut items = if let Some(dir) = self.data_dir.as_ref() {
            list_from_disk(dir, chat_id)
        } else {
            list_from_memory(&self.store, chat_id)
        };

        items.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| numeric_id(&b.id).cmp(&numeric_id(&a.id)))
        });
        items.truncate(limit.max(1));
        Ok(items)
    }

    fn read(&self, chat_id: &str, id: &str) -> Result<Option<ArtefactRecord>> {
        if !safe_store_id(chat_id) || !safe_store_id(id) {
            return Ok(None);
        }

        if let Some(dir) = self.data_dir.as_ref()
            && let Some(entry) = ArtefactStore::load_from_disk_for_chat(dir, chat_id, id)
        {
            return Ok(Some(record_from_entry(id, &entry)));
        }

        let cached = {
            let store = match self.store.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            store
                .items
                .get(id)
                .filter(|entry| entry.chat_id == chat_id)
                .map(|entry| record_from_entry(id, entry))
        };
        Ok(cached)
    }
}

fn list_from_memory(store: &Arc<Mutex<ArtefactStore>>, chat_id: &str) -> Vec<ArtefactSummary> {
    let store = match store.lock() {
        Ok(s) => s,
        Err(p) => p.into_inner(),
    };
    store
        .order
        .iter()
        .filter_map(|id| {
            store
                .items
                .get(id)
                .filter(|entry| entry.chat_id == chat_id)
                .map(|entry| summary_from_entry(id, entry))
        })
        .collect()
}

fn list_from_disk(data_dir: &Path, chat_id: &str) -> Vec<ArtefactSummary> {
    let sub = ArtefactStore::dir_for_chat(data_dir, chat_id);
    let Ok(read_dir) = std::fs::read_dir(&sub) else {
        return Vec::new();
    };

    read_dir
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let id = name.strip_suffix(".meta.json")?;
            if !safe_store_id(id) {
                return None;
            }
            summary_from_meta(&sub, id)
        })
        .collect()
}

fn summary_from_meta(sub: &Path, id: &str) -> Option<ArtefactSummary> {
    let meta_txt = std::fs::read_to_string(sub.join(format!("{id}.meta.json"))).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&meta_txt).ok()?;
    let bytes = std::fs::metadata(sub.join(format!("{id}.body")))
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    Some(ArtefactSummary {
        id: id.to_string(),
        chat_id: meta
            .get("chat_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        kind: meta
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("other")
            .to_string(),
        title: meta
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Artefact")
            .to_string(),
        bytes,
        created_at: meta.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0),
        tool_use_id: meta
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        metadata: meta.get("metadata").cloned().filter(|v| !v.is_null()),
    })
}

fn summary_from_entry(id: &str, entry: &ArtefactEntry) -> ArtefactSummary {
    ArtefactSummary {
        id: id.to_string(),
        chat_id: entry.chat_id.clone(),
        kind: kind_name(entry.kind).to_string(),
        title: entry.title.clone(),
        bytes: entry.content.len(),
        created_at: entry.created_at,
        tool_use_id: entry.tool_use_id.clone(),
        metadata: entry.metadata.clone(),
    }
}

fn record_from_entry(id: &str, entry: &ArtefactEntry) -> ArtefactRecord {
    ArtefactRecord {
        id: id.to_string(),
        chat_id: entry.chat_id.clone(),
        kind: kind_name(entry.kind).to_string(),
        title: entry.title.clone(),
        content: entry.content.clone(),
        mime_type: entry.mime_type.clone(),
        bytes: entry.content.len(),
        created_at: entry.created_at,
        tool_use_id: entry.tool_use_id.clone(),
        metadata: entry.metadata.clone(),
    }
}

fn kind_name(kind: ArtefactKind) -> &'static str {
    match kind {
        ArtefactKind::SecurityReview => "security_review",
        ArtefactKind::Image => "image",
        ArtefactKind::Other => "other",
    }
}

fn numeric_id(id: &str) -> u64 {
    id.strip_prefix('a')
        .and_then(|rest| rest.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(chat_id: &str, n: u64) -> ArtefactEntry {
        ArtefactEntry {
            chat_id: chat_id.to_string(),
            kind: ArtefactKind::SecurityReview,
            title: format!("report-{chat_id}"),
            content: format!("body-{chat_id}-{n}"),
            mime_type: "text/markdown".to_string(),
            metadata: None,
            tool_use_id: Some(format!("tool-{n}")),
            created_at: n,
        }
    }

    #[test]
    fn disk_reader_scopes_duplicate_ids_to_current_chat() {
        let dir = tempfile::tempdir().unwrap();
        ArtefactStore::persist_static(dir.path(), "a1", &entry("c-alpha", 1));
        ArtefactStore::persist_static(dir.path(), "a1", &entry("c-beta", 2));
        let reader = HttpArtefactReader::new(
            Arc::new(Mutex::new(ArtefactStore::default())),
            Some(dir.path().to_path_buf()),
        );

        let beta = reader.read("c-beta", "a1").unwrap().unwrap();
        assert_eq!(beta.chat_id, "c-beta");
        assert_eq!(beta.content, "body-c-beta-2");

        let alpha_list = reader.list("c-alpha", 10).unwrap();
        assert_eq!(alpha_list.len(), 1);
        assert_eq!(alpha_list[0].chat_id, "c-alpha");
        assert_eq!(alpha_list[0].title, "report-c-alpha");
    }

    #[test]
    fn memory_reader_scopes_to_current_chat() {
        let mut store = ArtefactStore::default();
        store.put("a1".to_string(), entry("c-alpha", 1));
        store.put("a2".to_string(), entry("c-beta", 2));
        let reader = HttpArtefactReader::new(Arc::new(Mutex::new(store)), None);

        let items = reader.list("c-alpha", 10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "a1");
        assert!(reader.read("c-alpha", "a2").unwrap().is_none());
    }
}
