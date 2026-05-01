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
use super::state::IngestConfig;
use super::stores::{ArtefactEntry, ArtefactStore, FileEntry, FileStore};
use super::wire::SseEvent;

pub(crate) struct SseOutput {
    /// Which chat this output is scoped to — stamped onto every
    /// artefact so `/api/conversations/<id>/artefacts` can filter.
    pub(crate) chat_id: String,
    /// Live broadcast channel for the chat.  Cloned from
    /// `ChatHandle.events` at construction; held here so each emit
    /// is a single send without re-locking the chats map.
    pub(crate) tx: broadcast::Sender<SseEvent>,
    /// Replay ring (cloned from `ChatHandle.replay`) — every emit
    /// pushes here so a reconnect can resume from a `Last-Event-ID`
    /// checkpoint.
    pub(crate) replay: Arc<std::sync::Mutex<super::state::EventRing>>,
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
    /// Live artefact-ingest target shared with `HttpState`.  A
    /// snapshot is taken in `send_artefact` and, when present,
    /// drives a fire-and-forget POST so swarm gets the bytes for
    /// durability + anonymous shares.  `None`-config or push
    /// failures never block the chat — the in-memory + on-disk
    /// caches the controller already maintains stay authoritative
    /// for the live UI read path.
    ///
    /// HTTP transport is the process-wide `crate::http::client()`
    /// singleton — no per-output client allocation; cloned at the
    /// spawn point so the spawned task owns its own handle.
    pub(crate) ingest: Arc<std::sync::Mutex<Option<IngestConfig>>>,
}

impl SseOutput {
    fn send(&self, evt: SseEvent) {
        // Push into the rolling replay ring first so a reconnecting
        // EventSource can resume from a `Last-Event-ID` checkpoint
        // even if no subscriber was attached when the event fired.
        // Recover from a poisoned mutex — a previous panic-while-
        // holder shouldn't permanently disable replay; the ring's
        // contents are still well-formed.
        {
            let mut ring = match self.replay.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            ring.push(evt.clone());
        }
        // Live broadcast — receiver-count errors are ignored: a turn
        // emits whether or not anyone is listening, the ring covers
        // late subscribers.
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
            parent_tool_id: None,
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
            parent_tool_id: None,
            tool_use_id: None,
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
                        path.display(),
                        m.len() / (1024 * 1024),
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
            self.next_file_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let inline_image = mime.starts_with("image/");
        let url = format!("/api/files/{id}");
        let bytes_len = bytes.len();
        let entry = FileEntry {
            bytes,
            mime: mime.clone(),
            name: name.clone(),
        };
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
            parent_tool_id: None,
        });

        // Every sent file is also an artefact — listing them in the
        // Artefacts tab makes a chat's generated outputs (images,
        // markdown reports, scripts, JSON dumps, …) discoverable after
        // the chat scroll has paged the original `file` block away.
        //   * Images: kind=Image, body=URL — reader renders <img>.
        //   * Markdown: kind=Other, body=UTF-8 text — reader feeds it
        //     straight into the existing markdown() render path, same
        //     as a SecurityReview artefact.  We re-read the FileEntry
        //     from the cache so we don't have to clone `bytes` twice.
        //   * Everything else: kind=Other, body=URL — reader shows a
        //     download card with name / mime / size.
        let is_markdown = mime == "text/markdown"
            || std::path::Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
                .unwrap_or(false);
        let kind = if inline_image {
            crate::message::ArtefactKind::Image
        } else {
            crate::message::ArtefactKind::Other
        };
        let content = if is_markdown {
            let s = match self.files.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            s.items
                .get(&id)
                .and_then(|e| std::str::from_utf8(&e.bytes).ok().map(|s| s.to_string()))
                .unwrap_or_else(|| url.clone())
        } else {
            url.clone()
        };
        // File-event mime stays as the OS's best guess (e.g. ".md" →
        // "text/plain") — the chat-history File block is happy with
        // that.  The *artefact* mime is the one that drives the
        // reader's branch, so promote markdown explicitly.
        let artefact_mime = if is_markdown {
            "text/markdown".to_string()
        } else {
            mime.clone()
        };
        let artefact = crate::message::Artefact {
            id: String::new(),
            kind,
            title: name.clone(),
            content,
            mime_type: artefact_mime.clone(),
            metadata: Some(serde_json::json!({
                "file_url": url,
                "file_name": name,
                "mime_type": artefact_mime,
                "bytes": bytes_len,
            })),
        };
        let _ = self.send_artefact(&artefact);

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
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = ArtefactEntry {
            chat_id: self.chat_id.clone(),
            kind: artefact.kind,
            title: artefact.title.clone(),
            content: artefact.content.clone(),
            mime_type: artefact.mime_type.clone(),
            metadata: artefact.metadata.clone(),
            tool_use_id: self.current_tool_use_id.clone(),
            created_at,
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
            id: id.clone(),
            kind: artefact.kind,
            title: artefact.title.clone(),
            url,
            bytes,
            metadata: artefact.metadata.clone(),
            parent_tool_id: None,
        });

        // Phase 3: fire-and-forget push to swarm so artefact bytes
        // outlive the cube and feed the swarm UI's artefact list.
        // Invariants:
        //   - never blocks the agent's output stream (spawn + return)
        //   - never propagates errors (logged on failure)
        //   - skips entirely when no ingest config is set (terminal /
        //     telegram / non-swarm dyson)
        let cfg_snapshot = match self.ingest.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };
        if let Some(cfg) = cfg_snapshot {
            // Resolve the body bytes the push should carry.  Three
            // cases mirror the reader-side branches the SPA already
            // understands (see send_file at L218):
            //   * Image / Other-with-URL-body: `content` is the
            //     `/api/files/<id>` URL — look up the FileEntry's
            //     bytes from the FileStore so swarm gets the actual
            //     binary, not the URL string.
            //   * Markdown / SecurityReview / other text: `content`
            //     is the inline body — use it directly.
            //   * Lookup failure: fall back to the content bytes
            //     (best-effort; the swarm side just sees the URL).
            let push_bytes: Vec<u8> = if artefact.content.starts_with("/api/files/") {
                let file_id = artefact
                    .content
                    .trim_start_matches("/api/files/")
                    .to_string();
                let from_store = match self.files.lock() {
                    Ok(g) => g.items.get(&file_id).map(|e| e.bytes.clone()),
                    Err(p) => p.into_inner().items.get(&file_id).map(|e| e.bytes.clone()),
                };
                from_store.unwrap_or_else(|| artefact.content.as_bytes().to_vec())
            } else {
                artefact.content.as_bytes().to_vec()
            };

            let chat_id = self.chat_id.clone();
            let artefact_id = id.clone();
            // Match the wire string swarm's `IngestRequest.kind`
            // expects.  `ArtefactKind` has #[serde(rename_all =
            // "snake_case")] so a serde detour would yield the same,
            // but the explicit match keeps the wire shape obvious
            // at the call site.
            let kind = match artefact.kind {
                crate::message::ArtefactKind::SecurityReview => "security_review",
                crate::message::ArtefactKind::Image => "image",
                crate::message::ArtefactKind::Other => "other",
            }
            .to_owned();
            let title = artefact.title.clone();
            let mime = artefact.mime_type.clone();
            let metadata = artefact.metadata.clone();
            tokio::spawn(async move {
                use base64::Engine as _;
                use base64::engine::general_purpose::STANDARD as B64;
                let payload = serde_json::json!({
                    "chat_id": chat_id,
                    "artefact_id": artefact_id,
                    "kind": kind,
                    "title": title,
                    "mime": mime,
                    "metadata": metadata,
                    "created_at": created_at as i64,
                    "body_b64": B64.encode(&push_bytes),
                });
                let r = crate::http::client()
                    .post(&cfg.url)
                    .bearer_auth(&cfg.token)
                    .timeout(std::time::Duration::from_secs(10))
                    .json(&payload)
                    .send()
                    .await;
                match r {
                    Ok(resp) if resp.status().is_success() => {
                        tracing::debug!(
                            artefact = %artefact_id,
                            chat = %chat_id,
                            "ingest: pushed",
                        );
                    }
                    Ok(resp) => {
                        tracing::warn!(
                            artefact = %artefact_id,
                            status = %resp.status(),
                            "ingest: swarm rejected artefact push",
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            artefact = %artefact_id,
                            error = %e,
                            "ingest: swarm push failed (network/timeout)",
                        );
                    }
                }
            });
        }
        Ok(())
    }

    fn error(&mut self, error: &DysonError) -> std::result::Result<(), DysonError> {
        // Sanitised message goes over the wire; full Display is written
        // to logs so operators still see the underlying IO/HTTP detail.
        // The wire path can be cross-tenant in OIDC deployments — paths
        // and upstream URLs leak across users without this guard.
        tracing::warn!(error = %error, "agent surfaced LLM error");
        self.send(SseEvent::LlmError {
            message: error.sanitized_message(),
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
        fixture_with_ingest(data_dir, None)
    }

    /// Variant that lets a test wire an `IngestConfig` through so the
    /// fire-and-forget push fires.  Tests that don't care about
    /// ingest call `fixture(...)` and get `None` (push path is
    /// silently skipped).
    fn fixture_with_ingest(
        data_dir: Option<PathBuf>,
        ingest: Option<IngestConfig>,
    ) -> (SseOutput, broadcast::Receiver<SseEvent>) {
        let (tx, rx) = broadcast::channel(64);
        let out = SseOutput {
            chat_id: "c-0001".to_string(),
            tx,
            replay: Arc::new(std::sync::Mutex::new(super::super::state::EventRing::new())),
            files: Arc::new(std::sync::Mutex::new(FileStore::default())),
            next_file_id: Arc::new(AtomicU64::new(1)),
            artefacts: Arc::new(std::sync::Mutex::new(ArtefactStore::default())),
            next_artefact_id: Arc::new(AtomicU64::new(1)),
            data_dir,
            current_tool_use_id: None,
            ingest: Arc::new(std::sync::Mutex::new(ingest)),
        };
        (out, rx)
    }

    #[test]
    fn text_delta_broadcasts_one_event() {
        let (mut out, mut rx) = fixture(None);
        out.text_delta("hello").unwrap();
        match rx.try_recv().unwrap() {
            SseEvent::Text { delta } => assert_eq!(delta, "hello"),
            other => panic!(
                "unexpected event: {other:?}",
                other = serde_json::to_string(&other).unwrap()
            ),
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
        assert!(
            out.current_tool_use_id.is_none(),
            "flush must clear the current tool"
        );
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
        let body =
            std::fs::read_to_string(dir.path().join("c-0001").join("artefacts").join("a1.body"))
                .unwrap();
        assert_eq!(body, "# Findings\n…");
    }

    #[test]
    fn send_file_falls_back_to_text_when_path_missing() {
        let (mut out, mut rx) = fixture(None);
        // Non-existent file: no panic, no file event — just a text
        // event so the user sees what happened.
        out.send_file(std::path::Path::new("/nonexistent/path-for-test.dat"))
            .unwrap();
        match rx.try_recv().unwrap() {
            SseEvent::Text { delta } => {
                assert!(delta.contains("/nonexistent/path-for-test.dat"));
                assert!(delta.contains("stat failed"));
            }
            _ => panic!("expected fallback text event"),
        }
    }

    #[test]
    fn send_file_emits_artefact_for_markdown_with_inlined_body() {
        // A sent .md file becomes an Other-kind artefact whose body is
        // the file's UTF-8 text — so the existing reader's markdown
        // render path picks it up without a second fetch.
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("findings.md");
        std::fs::write(&tmp, b"# Findings\n\n* a\n* b\n").unwrap();
        let (mut out, mut rx) = fixture(Some(dir.path().to_path_buf()));
        out.send_file(&tmp).unwrap();

        // First the file event (OS mime — text/plain for .md), then
        // the artefact event with the promoted markdown mime.
        match rx.try_recv().unwrap() {
            SseEvent::File { name, .. } => assert_eq!(name, "findings.md"),
            other => panic!(
                "expected file event, got {:?}",
                serde_json::to_string(&other).unwrap()
            ),
        }
        match rx.try_recv().unwrap() {
            SseEvent::Artefact {
                id,
                kind,
                title,
                metadata,
                ..
            } => {
                assert_eq!(id, "a1");
                assert_eq!(kind, ArtefactKind::Other);
                assert_eq!(title, "findings.md");
                let m = metadata.expect("metadata should be set");
                assert_eq!(m["file_name"], "findings.md");
                assert_eq!(
                    m["mime_type"], "text/markdown",
                    "metadata mime is the promoted markdown mime, not the OS guess"
                );
                assert_eq!(m["file_url"], "/api/files/f1");
                assert_eq!(m["bytes"].as_u64(), Some(20));
            }
            other => panic!(
                "expected artefact event, got {:?}",
                serde_json::to_string(&other).unwrap()
            ),
        }

        // The artefact body is the markdown text itself (not the URL);
        // that's what makes the existing reader Just Work.
        let body =
            std::fs::read_to_string(dir.path().join("c-0001").join("artefacts").join("a1.body"))
                .unwrap();
        assert_eq!(body, "# Findings\n\n* a\n* b\n");
    }

    #[test]
    fn send_file_emits_artefact_with_url_body_for_binary() {
        // Binary (non-image, non-markdown) files become Other-kind
        // artefacts whose body is the served URL — the reader uses
        // that to render a download card; we don't try to inline
        // arbitrary bytes.
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("data.bin");
        std::fs::write(&tmp, [0u8, 1, 2, 3, 0xFF]).unwrap();
        let (mut out, mut rx) = fixture(Some(dir.path().to_path_buf()));
        out.send_file(&tmp).unwrap();

        // Drop the file event, inspect the artefact.
        let _ = rx.try_recv().unwrap();
        match rx.try_recv().unwrap() {
            SseEvent::Artefact {
                kind,
                title,
                metadata,
                ..
            } => {
                assert_eq!(kind, ArtefactKind::Other);
                assert_eq!(title, "data.bin");
                let m = metadata.expect("metadata");
                assert_eq!(m["file_name"], "data.bin");
                // mime fell through to the binary default — what matters
                // is that the artefact metadata carries a usable file_url.
                assert_eq!(m["file_url"], "/api/files/f1");
            }
            other => panic!(
                "expected artefact event, got {:?}",
                serde_json::to_string(&other).unwrap()
            ),
        }
        let body =
            std::fs::read_to_string(dir.path().join("c-0001").join("artefacts").join("a1.body"))
                .unwrap();
        assert_eq!(body, "/api/files/f1", "binary artefact body is the URL");
    }

    #[test]
    fn send_file_emits_artefact_for_image() {
        // Pre-existing behaviour — images become Image-kind artefacts
        // with the served URL as body so the reader / chip can `<img>`.
        let dir = tempfile::tempdir().unwrap();
        // Minimal valid PNG header is enough — mime is sniffed by extension.
        let tmp = dir.path().join("pic.png");
        std::fs::write(&tmp, [0x89, b'P', b'N', b'G']).unwrap();
        let (mut out, mut rx) = fixture(Some(dir.path().to_path_buf()));
        out.send_file(&tmp).unwrap();
        let _ = rx.try_recv().unwrap();
        match rx.try_recv().unwrap() {
            SseEvent::Artefact { kind, .. } => assert_eq!(kind, ArtefactKind::Image),
            other => panic!(
                "expected artefact event, got {:?}",
                serde_json::to_string(&other).unwrap()
            ),
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
        assert_eq!(
            s.items.len(),
            1,
            "send_artefact must land despite poisoned lock"
        );
    }

    /// Spin up an in-process HTTP recorder bound to 127.0.0.1:0.
    /// Returns the URL plus a shared vec the test can read after the
    /// fire-and-forget push lands.  Each recorded entry is the raw
    /// JSON request body — tests assert on its structure.
    async fn ingest_recorder() -> (String, Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
        use hyper::body::Incoming;
        use hyper_util::rt::{TokioExecutor, TokioIo};
        let captured: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured_for_task = Arc::clone(&captured);
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let captured = Arc::clone(&captured_for_task);
                let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                    let captured = Arc::clone(&captured);
                    async move {
                        let bytes = http_body_util::BodyExt::collect(req.into_body())
                            .await
                            .map(|c| c.to_bytes())
                            .unwrap_or_default();
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                            captured.lock().unwrap().push(v);
                        }
                        Ok::<_, std::convert::Infallible>(
                            hyper::Response::builder()
                                .status(204)
                                .body(http_body_util::Empty::<hyper::body::Bytes>::new())
                                .unwrap(),
                        )
                    }
                });
                tokio::spawn(async move {
                    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        (format!("http://{addr}/ingest"), captured)
    }

    #[tokio::test(flavor = "current_thread", start_paused = false)]
    async fn send_artefact_skips_push_when_no_ingest_config() {
        // Phase 3: with no IngestConfig wired, send_artefact must
        // emit + persist as before and NOT spawn any push task.  We
        // can't introspect the runtime's task list — but a fixture
        // built without ingest will not call into the push branch,
        // and no panic / no extra event past Artefact is the
        // observable contract.
        let dir = tempfile::tempdir().unwrap();
        let (mut out, mut rx) = fixture(Some(dir.path().to_path_buf()));
        let art = Artefact {
            id: String::new(),
            kind: ArtefactKind::SecurityReview,
            title: "no-push.md".into(),
            content: "## Findings".into(),
            mime_type: "text/markdown".into(),
            metadata: None,
        };
        out.send_artefact(&art).unwrap();
        match rx.try_recv().unwrap() {
            SseEvent::Artefact { id, .. } => assert_eq!(id, "a1"),
            other => panic!(
                "expected artefact event, got {}",
                serde_json::to_string(&other).unwrap()
            ),
        }
        // No further events queued — push branch was correctly skipped.
        assert!(rx.try_recv().is_err(), "no extra events");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_artefact_pushes_inline_text_to_swarm() {
        // Phase 3 happy path: with an IngestConfig pointing at our
        // recorder, send_artefact spawns a fire-and-forget POST.  We
        // assert on the recorded JSON envelope: chat_id, artefact_id,
        // kind (snake_case wire string), title, mime, base64 body
        // round-trips back to the input plaintext.
        use base64::Engine;
        let (url, captured) = ingest_recorder().await;
        let dir = tempfile::tempdir().unwrap();
        let (mut out, _rx) = fixture_with_ingest(
            Some(dir.path().to_path_buf()),
            Some(IngestConfig {
                url,
                token: "it_test".into(),
            }),
        );
        let art = Artefact {
            id: String::new(),
            kind: ArtefactKind::SecurityReview,
            title: "report.md".into(),
            content: "## Findings\n\n* one".into(),
            mime_type: "text/markdown".into(),
            metadata: Some(serde_json::json!({"k": "v"})),
        };
        out.send_artefact(&art).unwrap();

        // Wait up to 2s for the spawned push to land at the recorder.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if !captured.lock().unwrap().is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("ingest push did not land at recorder within 2s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let captured = captured.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "exactly one push fired");
        let body = &captured[0];
        assert_eq!(body["chat_id"], "c-0001");
        assert_eq!(body["artefact_id"], "a1");
        assert_eq!(body["kind"], "security_review");
        assert_eq!(body["title"], "report.md");
        assert_eq!(body["mime"], "text/markdown");
        assert_eq!(body["metadata"]["k"], "v");
        let b64 = body["body_b64"].as_str().expect("body_b64 string");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(decoded, b"## Findings\n\n* one");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_artefact_push_uses_filestore_bytes_for_url_body() {
        // For Image / Other-with-URL-body artefacts (`content` is the
        // /api/files/<id> deeplink), the push must resolve the bytes
        // out of the FileStore, not send the URL string.  We seed a
        // FileEntry directly, build an artefact whose content points
        // at it, and verify the recorded push carries the binary
        // bytes base64-encoded.
        use base64::Engine;
        let (url, captured) = ingest_recorder().await;
        let (mut out, _rx) = fixture_with_ingest(
            None,
            Some(IngestConfig {
                url,
                token: "it_test".into(),
            }),
        );
        // Seed a binary FileEntry for id "f1".
        out.files.lock().unwrap().put(
            "f1".into(),
            FileEntry {
                bytes: vec![0xDE, 0xAD, 0xBE, 0xEF],
                mime: "application/octet-stream".into(),
                name: "blob.bin".into(),
            },
        );
        let art = Artefact {
            id: String::new(),
            kind: ArtefactKind::Other,
            title: "blob.bin".into(),
            content: "/api/files/f1".into(),
            mime_type: "application/octet-stream".into(),
            metadata: None,
        };
        out.send_artefact(&art).unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if !captured.lock().unwrap().is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("ingest push did not land at recorder within 2s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let captured = captured.lock().unwrap().clone();
        let body = &captured[0];
        let b64 = body["body_b64"].as_str().unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(
            decoded,
            vec![0xDE, 0xAD, 0xBE, 0xEF],
            "URL-body artefact must push the FileStore bytes, not the URL string",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_artefact_swallows_push_failure() {
        // When the swarm endpoint is unreachable / 5xx, the push task
        // logs and returns — send_artefact itself must still succeed
        // (returns Ok(())) and the SSE Artefact event must still
        // emit.  Caller (the agent) sees a normal completion.
        // Simulate "endpoint unreachable" by pointing at a port we
        // explicitly don't bind.  reqwest's connect-refused happens
        // in the spawned task; the test just verifies send_artefact
        // returned Ok and the event landed.
        let dir = tempfile::tempdir().unwrap();
        let (mut out, mut rx) = fixture_with_ingest(
            Some(dir.path().to_path_buf()),
            Some(IngestConfig {
                url: "http://127.0.0.1:1/nope".into(),
                token: "it_test".into(),
            }),
        );
        let art = Artefact {
            id: String::new(),
            kind: ArtefactKind::SecurityReview,
            title: "doomed.md".into(),
            content: "won't reach swarm".into(),
            mime_type: "text/markdown".into(),
            metadata: None,
        };
        // The call must NOT propagate the network failure.
        out.send_artefact(&art)
            .expect("send_artefact must not error on push failure");
        // SSE event still landed.
        match rx.try_recv().unwrap() {
            SseEvent::Artefact { id, title, .. } => {
                assert_eq!(id, "a1");
                assert_eq!(title, "doomed.md");
            }
            other => panic!(
                "expected artefact event, got {}",
                serde_json::to_string(&other).unwrap()
            ),
        }
        // Brief wait for the spawned task to fail; nothing observable
        // to check here besides "we didn't crash".  Logs land in
        // captured tracing but tests don't gate on them.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
