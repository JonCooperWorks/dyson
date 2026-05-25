use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::Result;
use crate::message::ArtefactKind;
use crate::tool::artefacts::{
    ArtefactLookup, ArtefactReader, ArtefactRecord, ArtefactSummary, safe_store_id,
};

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
    fn list(&self, chat_id: Option<&str>, limit: usize) -> Result<Vec<ArtefactSummary>> {
        if let Some(chat_id) = chat_id
            && !safe_store_id(chat_id)
        {
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

    fn read(&self, chat_id: Option<&str>, id: &str) -> Result<ArtefactLookup> {
        if let Some(chat_id) = chat_id
            && !safe_store_id(chat_id)
        {
            return Ok(ArtefactLookup::Missing);
        }
        if !safe_store_id(id) {
            return Ok(ArtefactLookup::Missing);
        }

        if let Some(dir) = self.data_dir.as_ref() {
            if let Some(chat_id) = chat_id {
                if let Some(entry) = ArtefactStore::load_from_disk_for_chat(dir, chat_id, id) {
                    return Ok(ArtefactLookup::Found(record_from_entry(id, &entry)));
                }
            } else {
                let records = records_from_disk(dir, id);
                match records.as_slice() {
                    [] => {}
                    [record] => return Ok(ArtefactLookup::Found(record.clone())),
                    _ => {
                        let summaries = records.iter().map(summary_from_record).collect();
                        return Ok(ArtefactLookup::Ambiguous(summaries));
                    }
                }
            }
        }

        let records = {
            let store = match self.store.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            records_from_memory(&store, chat_id, id)
        };
        Ok(match records.as_slice() {
            [] => ArtefactLookup::Missing,
            [record] => ArtefactLookup::Found(record.clone()),
            _ => ArtefactLookup::Ambiguous(records.iter().map(summary_from_record).collect()),
        })
    }
}

fn list_from_memory(
    store: &Arc<Mutex<ArtefactStore>>,
    chat_id: Option<&str>,
) -> Vec<ArtefactSummary> {
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
                .filter(|entry| chat_id.is_none_or(|chat_id| entry.chat_id == chat_id))
                .map(|entry| summary_from_entry(id, entry))
        })
        .collect()
}

fn list_from_disk(data_dir: &Path, chat_id: Option<&str>) -> Vec<ArtefactSummary> {
    if let Some(chat_id) = chat_id {
        return list_from_disk_subdir(&ArtefactStore::dir_for_chat(data_dir, chat_id));
    }

    let Ok(read_dir) = std::fs::read_dir(data_dir) else {
        return Vec::new();
    };
    read_dir
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            Some(list_from_disk_subdir(&path.join("artefacts")))
        })
        .flatten()
        .collect()
}

fn list_from_disk_subdir(sub: &Path) -> Vec<ArtefactSummary> {
    let Ok(read_dir) = std::fs::read_dir(sub) else {
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
            summary_from_meta(sub, id)
        })
        .collect()
}

fn records_from_disk(data_dir: &Path, id: &str) -> Vec<ArtefactRecord> {
    let Ok(read_dir) = std::fs::read_dir(data_dir) else {
        return Vec::new();
    };
    read_dir
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let sub = path.join("artefacts");
            if !sub.join(format!("{id}.meta.json")).exists() {
                return None;
            }
            let chat_id = path.file_name()?.to_str()?;
            ArtefactStore::load_from_disk_for_chat(data_dir, chat_id, id)
                .map(|entry| record_from_entry(id, &entry))
        })
        .collect()
}

fn records_from_memory(
    store: &ArtefactStore,
    chat_id: Option<&str>,
    id: &str,
) -> Vec<ArtefactRecord> {
    store
        .items
        .get(id)
        .filter(|entry| chat_id.is_none_or(|chat_id| entry.chat_id == chat_id))
        .map(|entry| vec![record_from_entry(id, entry)])
        .unwrap_or_default()
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

fn summary_from_record(record: &ArtefactRecord) -> ArtefactSummary {
    ArtefactSummary {
        id: record.id.clone(),
        chat_id: record.chat_id.clone(),
        kind: record.kind.clone(),
        title: record.title.clone(),
        bytes: record.bytes,
        created_at: record.created_at,
        tool_use_id: record.tool_use_id.clone(),
        metadata: record.metadata.clone(),
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
    fn disk_reader_lists_whole_instance_and_scopes_when_requested() {
        let dir = tempfile::tempdir().unwrap();
        ArtefactStore::persist_static(dir.path(), "a1", &entry("c-alpha", 1));
        ArtefactStore::persist_static(dir.path(), "a1", &entry("c-beta", 2));
        let reader = HttpArtefactReader::new(
            Arc::new(Mutex::new(ArtefactStore::default())),
            Some(dir.path().to_path_buf()),
        );

        let all = reader.list(None, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|a| a.chat_id == "c-alpha"));
        assert!(all.iter().any(|a| a.chat_id == "c-beta"));

        let alpha_list = reader.list(Some("c-alpha"), 10).unwrap();
        assert_eq!(alpha_list.len(), 1);
        assert_eq!(alpha_list[0].chat_id, "c-alpha");
        assert_eq!(alpha_list[0].title, "report-c-alpha");
    }

    #[test]
    fn disk_reader_reports_ambiguous_read_without_chat_scope() {
        let dir = tempfile::tempdir().unwrap();
        ArtefactStore::persist_static(dir.path(), "a1", &entry("c-alpha", 1));
        ArtefactStore::persist_static(dir.path(), "a1", &entry("c-beta", 2));
        let reader = HttpArtefactReader::new(
            Arc::new(Mutex::new(ArtefactStore::default())),
            Some(dir.path().to_path_buf()),
        );

        let lookup = reader.read(None, "a1").unwrap();
        let ArtefactLookup::Ambiguous(matches) = lookup else {
            panic!("expected ambiguous lookup");
        };
        assert_eq!(matches.len(), 2);
        assert!(matches.iter().any(|a| a.chat_id == "c-alpha"));
        assert!(matches.iter().any(|a| a.chat_id == "c-beta"));

        let scoped = reader.read(Some("c-beta"), "a1").unwrap();
        let ArtefactLookup::Found(beta) = scoped else {
            panic!("expected scoped hit");
        };
        assert_eq!(beta.chat_id, "c-beta");
        assert_eq!(beta.content, "body-c-beta-2");
    }

    #[test]
    fn memory_reader_lists_instance_and_scopes_when_requested() {
        let mut store = ArtefactStore::default();
        store.put("a1".to_string(), entry("c-alpha", 1));
        store.put("a2".to_string(), entry("c-beta", 2));
        let reader = HttpArtefactReader::new(Arc::new(Mutex::new(store)), None);

        let all = reader.list(None, 10).unwrap();
        assert_eq!(all.len(), 2);

        let items = reader.list(Some("c-alpha"), 10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "a1");
        assert!(matches!(
            reader.read(Some("c-alpha"), "a2").unwrap(),
            ArtefactLookup::Missing
        ));
        assert!(matches!(
            reader.read(None, "a2").unwrap(),
            ArtefactLookup::Found(record) if record.chat_id == "c-beta"
        ));
    }
}
