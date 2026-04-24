// ===========================================================================
// HTTP controller — `SseOutput`, the per-turn `Output` adapter.
//
// The agent loop drives `Output` to emit text deltas, tool starts, tool
// results, files, artefacts, etc.  `SseOutput` fans each call into one
// `SseEvent` and pushes it through the chat's broadcast channel; any
// `EventSource` subscriber on `/events` sees the frame.  `send_file` and
// `send_artefact` also park bytes/metadata in the controller-wide
// `FileStore` / `ArtefactStore` (write-through to disk when configured)
// so cold reload + cache-miss paths can serve them later.
// ===========================================================================

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::error::DysonError;
use crate::tool::{CheckpointEvent, ToolOutput};

use super::Output;
use super::responses::mime_for_extension;
use super::stores::{ArtefactEntry, ArtefactStore, FileEntry, FileStore};
use super::wire::SseEvent;

pub(crate) struct SseOutput {
    /// Which chat this output is scoped to — stamped onto every
    /// artefact so `/api/conversations/<id>/artefacts` can filter.
    pub(crate) chat_id: String,
    pub(crate) tx: broadcast::Sender<SseEvent>,
    /// Shared file store so `send_file` can stash agent-produced bytes
    /// for the UI to fetch via `/api/files/<id>`.
    pub(crate) files: Arc<std::sync::Mutex<FileStore>>,
    /// Counter for synthesising file ids when the agent attaches an
    /// unnamed file.  Wraps; collisions are vanishingly unlikely
    /// inside the FileStore::MAX_FILES window.
    pub(crate) next_file_id: Arc<std::sync::atomic::AtomicU64>,
    /// Shared artefact store for `send_artefact`.
    pub(crate) artefacts: Arc<std::sync::Mutex<ArtefactStore>>,
    pub(crate) next_artefact_id: Arc<std::sync::atomic::AtomicU64>,
    /// Optional write-through disk directory for persistence.
    pub(crate) data_dir: Option<PathBuf>,
    /// Currently-executing tool's id.  Set in `tool_use_start` and
    /// carried across subsequent `send_file` / `send_artefact` calls
    /// that the agent loop triggers from the same `ToolOutput`.  The
    /// id is stamped on every file and artefact entry so the UI can
    /// wire image artefacts back to the originating tool panel on
    /// chat reload.
    pub(crate) current_tool_use_id: Option<String>,
}

impl SseOutput {
    fn send(&self, evt: SseEvent) {
        // Ignore receiver-count errors — there may be no subscribers right
        // now; events are still useful when one connects mid-turn (only the
        // most recent N stay buffered, that's fine for SSE semantics).
        let _ = self.tx.send(evt);
    }
}

impl Output for SseOutput {
    fn text_delta(&mut self, text: &str) -> std::result::Result<(), DysonError> {
        self.send(SseEvent::Text {
            delta: text.to_string(),
        });
        Ok(())
    }

    fn thinking_delta(&mut self, text: &str) -> std::result::Result<(), DysonError> {
        self.send(SseEvent::Thinking {
            delta: text.to_string(),
        });
        Ok(())
    }

    fn tool_use_start(&mut self, id: &str, name: &str) -> std::result::Result<(), DysonError> {
        // Remember which tool is running so any `send_file` /
        // `send_artefact` calls that follow this turn's `tool_result`
        // can stamp the same id on their FileEntry / ArtefactEntry.
        // Reset in `flush` at turn end; NOT reset in `tool_result` —
        // files are emitted AFTER the tool result per execution.rs.
        self.current_tool_use_id = Some(id.to_string());
        self.send(SseEvent::ToolStart {
            id: id.to_string(),
            name: name.to_string(),
        });
        Ok(())
    }

    fn tool_use_complete(&mut self) -> std::result::Result<(), DysonError> {
        Ok(())
    }

    fn tool_result(&mut self, output: &ToolOutput) -> std::result::Result<(), DysonError> {
        self.send(SseEvent::ToolResult {
            content: output.content.clone(),
            is_error: output.is_error,
            view: output.view.clone(),
        });
        Ok(())
    }

    fn send_file(&mut self, path: &std::path::Path) -> std::result::Result<(), DysonError> {
        // Slurp the file (size-capped to keep a runaway tool from
        // blowing memory), park it in the shared FileStore, emit an
        // SSE `file` event with the URL the UI fetches.
        const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
        match std::fs::metadata(path) {
            Ok(m) if m.len() > MAX_FILE_BYTES => {
                self.send(SseEvent::Text {
                    delta: format!(
                        "\n[file: {} too large ({} MB) — not delivered]\n",
                        path.display(), m.len() / (1024 * 1024),
                    ),
                });
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                self.send(SseEvent::Text {
                    delta: format!("\n[file: {} — stat failed: {e}]\n", path.display()),
                });
                return Ok(());
            }
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.send(SseEvent::Text {
                    delta: format!("\n[file: {} — read failed: {e}]\n", path.display()),
                });
                return Ok(());
            }
        };
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        let mime = mime_for_extension(path);
        let id = format!(
            "f{}",
            self.next_file_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let inline_image = mime.starts_with("image/");
        let url = format!("/api/files/{id}");
        let bytes_len = bytes.len();
        let entry = FileEntry { bytes, mime: mime.clone(), name: name.clone() };
        // Write-through to disk first so a controller crash between
        // the memory put and the disk write doesn't leak a dangling
        // in-memory reference that can't be rehydrated.
        if let Some(dir) = self.data_dir.as_ref() {
            FileStore::persist_static(dir, &id, &entry);
        }
        // std::sync::Mutex — blocking but the critical section is a
        // HashMap insert + a Vec push.  Negligible contention.
        // Recover from poisoning so a panicked previous holder doesn't
        // silently disable the cache for the rest of the process.
        let mut s = match self.files.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        s.put(id.clone(), entry);
        drop(s);
        self.send(SseEvent::File {
            name: name.clone(),
            mime_type: mime.clone(),
            url: url.clone(),
            inline_image,
        });

        // Images are also artefacts — listing them in the Artefacts
        // tab makes a chat's generated images discoverable after the
        // original chat scroll has paged them away.  The body here is
        // the served URL (not the raw bytes); the reader notices the
        // `image/*` mime and renders with `<img>` instead of markdown.
        if inline_image {
            let artefact = crate::message::Artefact {
                id: String::new(),
                kind: crate::message::ArtefactKind::Image,
                title: name.clone(),
                content: url.clone(),
                mime_type: mime.clone(),
                metadata: Some(serde_json::json!({
                    "file_url": url,
                    "file_name": name,
                    "bytes": bytes_len,
                })),
            };
            let _ = self.send_artefact(&artefact);
        }

        Ok(())
    }

    fn checkpoint(&mut self, event: &CheckpointEvent) -> std::result::Result<(), DysonError> {
        // CheckpointEvent has no Display impl — Debug suffices for the
        // UI's progress feed in v1.  Replace with a typed event later.
        self.send(SseEvent::Checkpoint {
            text: format!("{event:?}"),
        });
        Ok(())
    }

    fn send_artefact(
        &mut self,
        artefact: &crate::message::Artefact,
    ) -> std::result::Result<(), DysonError> {
        let id = format!(
            "a{}",
            self.next_artefact_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // The `url` surfaces to the client as the chip's href — use the
        // SPA deep-link so cmd-click / copy-paste lands on the reader.
        // The raw bytes still live at `/api/artefacts/<id>` and the
        // reader fetches them itself once mounted.
        let url = format!("/#/artefacts/{id}");
        let bytes = artefact.content.len();
        let entry = ArtefactEntry {
            chat_id: self.chat_id.clone(),
            kind: artefact.kind,
            title: artefact.title.clone(),
            content: artefact.content.clone(),
            mime_type: artefact.mime_type.clone(),
            metadata: artefact.metadata.clone(),
            tool_use_id: self.current_tool_use_id.clone(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        if let Some(dir) = self.data_dir.as_ref() {
            ArtefactStore::persist_static(dir, &id, &entry);
        }
        let mut s = match self.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        s.put(id.clone(), entry);
        drop(s);
        self.send(SseEvent::Artefact {
            id,
            kind: artefact.kind,
            title: artefact.title.clone(),
            url,
            bytes,
            metadata: artefact.metadata.clone(),
        });
        Ok(())
    }

    fn error(&mut self, error: &DysonError) -> std::result::Result<(), DysonError> {
        self.send(SseEvent::LlmError {
            message: error.to_string(),
        });
        Ok(())
    }

    fn flush(&mut self) -> std::result::Result<(), DysonError> {
        // End of turn — the next turn's `tool_use_start` will set a
        // new id; until then there's no "current" tool.
        self.current_tool_use_id = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Artefact, ArtefactKind};
    use std::sync::atomic::AtomicU64;

    fn fixture(data_dir: Option<PathBuf>) -> (SseOutput, broadcast::Receiver<SseEvent>) {
        let (tx, rx) = broadcast::channel(64);
        let out = SseOutput {
            chat_id: "c-0001".to_string(),
            tx,
            files: Arc::new(std::sync::Mutex::new(FileStore::default())),
            next_file_id: Arc::new(AtomicU64::new(1)),
            artefacts: Arc::new(std::sync::Mutex::new(ArtefactStore::default())),
            next_artefact_id: Arc::new(AtomicU64::new(1)),
            data_dir,
            current_tool_use_id: None,
        };
        (out, rx)
    }

    #[test]
    fn text_delta_broadcasts_one_event() {
        let (mut out, mut rx) = fixture(None);
        out.text_delta("hello").unwrap();
        match rx.try_recv().unwrap() {
            SseEvent::Text { delta } => assert_eq!(delta, "hello"),
            other => panic!("unexpected event: {other:?}", other = serde_json::to_string(&other).unwrap()),
        }
    }

    #[test]
    fn tool_use_start_remembers_id_until_flush() {
        // The id stamped on subsequent send_file / send_artefact calls
        // is what wires an image_generate output to its tool panel on
        // refresh.  Drop it on flush so the next turn starts clean.
        let (mut out, _rx) = fixture(None);
        out.tool_use_start("tool_42", "image_generate").unwrap();
        assert_eq!(out.current_tool_use_id.as_deref(), Some("tool_42"));
        out.flush().unwrap();
        assert!(out.current_tool_use_id.is_none(), "flush must clear the current tool");
    }

    #[test]
    fn send_artefact_writes_through_to_disk_and_emits_event() {
        let dir = tempfile::tempdir().unwrap();
        let (mut out, mut rx) = fixture(Some(dir.path().to_path_buf()));
        let art = Artefact {
            id: String::new(),
            kind: ArtefactKind::SecurityReview,
            title: "review.md".to_string(),
            content: "# Findings\n…".to_string(),
            mime_type: "text/markdown".to_string(),
            metadata: None,
        };
        out.send_artefact(&art).unwrap();
        // SSE event mints id `a1`.
        match rx.try_recv().unwrap() {
            SseEvent::Artefact { id, title, .. } => {
                assert_eq!(id, "a1");
                assert_eq!(title, "review.md");
            }
            _ => panic!("expected artefact event"),
        }
        // On-disk write-through under the chat's artefacts dir.
        let body = std::fs::read_to_string(
            dir.path().join("c-0001").join("artefacts").join("a1.body")
        ).unwrap();
        assert_eq!(body, "# Findings\n…");
    }

    #[test]
    fn send_file_falls_back_to_text_when_path_missing() {
        let (mut out, mut rx) = fixture(None);
        // Non-existent file: no panic, no file event — just a text
        // event so the user sees what happened.
        out.send_file(std::path::Path::new("/nonexistent/path-for-test.dat")).unwrap();
        match rx.try_recv().unwrap() {
            SseEvent::Text { delta } => {
                assert!(delta.contains("/nonexistent/path-for-test.dat"));
                assert!(delta.contains("stat failed"));
            }
            _ => panic!("expected fallback text event"),
        }
    }

    #[test]
    fn send_artefact_recovers_from_poisoned_artefact_lock() {
        // Regression: the cache write used to be `if let Ok(mut s) =
        // self.artefacts.lock() { s.put(...) }`, which silently dropped
        // the insert if the lock was poisoned.  After a single panic-
        // while-holding-lock anywhere in the process, every subsequent
        // emission would be invisible to the in-memory index, even
        // though the disk write still happened.
        //
        // Drive the failure shape directly: poison the lock by panicking
        // in a thread that holds it, then verify a subsequent
        // send_artefact still lands in the cache.
        let dir = tempfile::tempdir().unwrap();
        let (mut out, _rx) = fixture(Some(dir.path().to_path_buf()));
        // Poison: spawn a thread that locks the artefacts mutex and
        // panics, then join it.
        let artefacts = out.artefacts.clone();
        let h = std::thread::spawn(move || {
            let _guard = artefacts.lock().unwrap();
            panic!("intentional poisoning");
        });
        let _ = h.join();
        assert!(out.artefacts.is_poisoned(), "lock must be poisoned");

        // Send an artefact through the now-poisoned mutex.  Must not
        // panic, must end up in the cache.
        let art = Artefact {
            id: String::new(),
            kind: ArtefactKind::SecurityReview,
            title: "post-panic.md".to_string(),
            content: "still works".to_string(),
            mime_type: "text/markdown".to_string(),
            metadata: None,
        };
        out.send_artefact(&art).unwrap();
        let s = match out.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        assert_eq!(s.items.len(), 1, "send_artefact must land despite poisoned lock");
    }
}
