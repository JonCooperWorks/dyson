use super::SseEvent;
use super::*;

#[test]
fn new_ring_does_not_preallocate() {
    // MEM-01: every ChatHandle owns a ring; reserving all 4096 slots
    // up front cost hundreds of KB per idle chat.
    assert_eq!(EventRing::new().entries.capacity(), 0);
}
// Stand-alone tests for the rolling buffer — no async, no
// tokio.  Asserts the FIFO bound and the `since` cutoff that
// the SSE replay path depends on.
#[test]
fn event_ring_evicts_oldest_at_cap() {
    let mut r = EventRing::new();
    let cap = EventRing::CAP;
    for n in 0..(cap + 5) {
        r.push(SseEvent::Text {
            delta: format!("e{n}"),
        });
    }
    assert_eq!(r.entries.len(), cap, "ring must hold the cap");
    // Earliest entry id is 6 (5 evicted + 1-based).
    assert_eq!(r.entries.front().map(|(i, _)| *i), Some(6));
    assert_eq!(r.entries.back().map(|(i, _)| *i), Some((cap + 5) as u64));
}

#[test]
fn event_ring_since_skips_seen_events() {
    let mut r = EventRing::new();
    for n in 0..5 {
        r.push(SseEvent::Text {
            delta: format!("e{n}"),
        });
    }
    let after = r.since(2);
    let ids: Vec<u64> = after.iter().map(|(i, _)| *i).collect();
    assert_eq!(ids, vec![3, 4, 5]);
    assert!(r.since(99).is_empty(), "since past tip yields nothing");
}

#[test]
fn event_ring_clear_drops_entries_but_keeps_id_monotonic() {
    // The ring is per-turn state.  At end-of-turn we wipe the
    // entries so a fresh EventSource opening for the *next* turn
    // (which sends no Last-Event-ID) doesn't replay the previous
    // turn's text deltas into the new agent placeholder — that's
    // the visible "subsequent message responds with the last
    // message" duplication bug.
    let mut r = EventRing::new();
    r.push(SseEvent::Text {
        delta: "stale-a".into(),
    });
    let last_done = r.push(SseEvent::Done);
    assert_eq!(r.since(0).len(), 2, "preconditions");

    r.clear();
    assert!(r.entries.is_empty(), "clear must wipe entries");
    assert!(
        r.since(0).is_empty(),
        "since(0) returns nothing after clear"
    );

    // next_id stays monotonic so any live subscriber that's still
    // around (e.g. the previous turn's ES racing to receive Done)
    // doesn't observe a regressed id from a future emit.
    let next = r.push(SseEvent::Text {
        delta: "fresh".into(),
    });
    assert!(
        next > last_done,
        "next_id must remain monotonic across clear ({next} <= {last_done})"
    );
}

#[tokio::test]
async fn mid_turn_text_admission_persists_empty_queue_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let handle = ChatHandle::new("c-test".to_string(), "test".to_string(), Some(tmp.path()));
    handle
        .enqueue_turn(QueuedTurn {
            prompt: "while you work".to_string(),
            attachments: Vec::new(),
            provider: None,
            model: None,
            queue_mode: Some("next_tool_call".to_string()),
        })
        .await;
    let qpath = handle.queue_path.clone().unwrap();
    assert!(qpath.exists(), "enqueue persists queue file");

    let mut admitted = Vec::new();
    let count = handle
        .admit_text_only_queued_turns(|message| {
            admitted.push(message);
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(count, 1);
    assert_eq!(admitted.len(), 1);
    assert!(
        !qpath.exists(),
        "admitting the last queued turn removes queue.json"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn next_tool_call_admission_consumes_before_callback_so_it_cannot_repeat() {
    let tmp = tempfile::tempdir().unwrap();
    let handle = Arc::new(ChatHandle::new(
        "c-test".to_string(),
        "test".to_string(),
        Some(tmp.path()),
    ));
    handle
        .enqueue_turn(QueuedTurn {
            prompt: "only once".to_string(),
            attachments: Vec::new(),
            provider: None,
            model: None,
            queue_mode: Some("next_tool_call".to_string()),
        })
        .await;

    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let release_gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

    let first_handle = Arc::clone(&handle);
    let first_release_gate = Arc::clone(&release_gate);
    let first = tokio::spawn(async move {
        let mut admitted = Vec::new();
        let count = first_handle
            .admit_text_only_queued_turns(|message| {
                admitted.push(message);
                started_tx.send(()).unwrap();
                let (lock, cvar) = &*first_release_gate;
                let released = lock.lock().unwrap();
                let (_released, timeout) = cvar
                    .wait_timeout_while(released, std::time::Duration::from_secs(2), |released| {
                        !*released
                    })
                    .unwrap();
                assert!(
                    !timeout.timed_out(),
                    "test timed out waiting to release first admit"
                );
                Ok(())
            })
            .await
            .unwrap();
        (count, admitted.len())
    });

    started_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();

    let mut second_admitted = Vec::new();
    let second_count = handle
        .admit_text_only_queued_turns(|message| {
            second_admitted.push(message);
            Ok(())
        })
        .await
        .unwrap();

    let (lock, cvar) = &*release_gate;
    *lock.lock().unwrap() = true;
    cvar.notify_one();
    let (first_count, first_admitted) = first.await.unwrap();

    assert_eq!(first_count, 1);
    assert_eq!(first_admitted, 1);
    assert_eq!(
        second_count, 0,
        "a next-tool queued turn must be invisible to later admission callbacks once admitted"
    );
    assert!(second_admitted.is_empty());
    assert_eq!(handle.queued.lock().unwrap().len(), 0);
    assert!(!handle.queue_path.clone().unwrap().exists());
}

#[tokio::test]
async fn multiple_next_tool_call_turns_admit_fifo_and_only_once() {
    let tmp = tempfile::tempdir().unwrap();
    let handle = ChatHandle::new("c-test".to_string(), "test".to_string(), Some(tmp.path()));
    for prompt in ["first", "second"] {
        handle
            .enqueue_turn(QueuedTurn {
                prompt: prompt.to_string(),
                attachments: Vec::new(),
                provider: None,
                model: None,
                queue_mode: Some("next_tool_call".to_string()),
            })
            .await;
    }

    let mut admitted = Vec::new();
    let count = handle
        .admit_text_only_queued_turns(|message| {
            if let Some(dyson_core::message::ContentBlock::Text { text }) = message.content.first()
            {
                admitted.push(text.clone());
            }
            Ok(())
        })
        .await
        .unwrap();

    let mut repeated = Vec::new();
    let repeated_count = handle
        .admit_text_only_queued_turns(|message| {
            repeated.push(message);
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(count, 2);
    assert_eq!(admitted, vec!["first", "second"]);
    assert_eq!(repeated_count, 0);
    assert!(repeated.is_empty());
    assert_eq!(handle.queued.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn normal_text_queued_turns_keep_legacy_mid_turn_admission() {
    let tmp = tempfile::tempdir().unwrap();
    let handle = ChatHandle::new("c-test".to_string(), "test".to_string(), Some(tmp.path()));
    handle
        .enqueue_turn(QueuedTurn {
            prompt: "later".to_string(),
            attachments: Vec::new(),
            provider: None,
            model: None,
            queue_mode: Some("normal".to_string()),
        })
        .await;

    let mut admitted = Vec::new();
    let count = handle
        .admit_text_only_queued_turns(|message| {
            admitted.push(message);
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(count, 1);
    assert_eq!(admitted.len(), 1);
    assert_eq!(handle.queued.lock().unwrap().len(), 0);
    assert!(!handle.queue_path.clone().unwrap().exists());
}

#[tokio::test]
async fn mid_turn_admission_leaves_attachment_turns_for_end_of_turn_drain() {
    let tmp = tempfile::tempdir().unwrap();
    let handle = ChatHandle::new("c-test".to_string(), "test".to_string(), Some(tmp.path()));
    handle
        .enqueue_turn(QueuedTurn {
            prompt: "see attached".to_string(),
            attachments: vec![QueuedAttachment {
                mime_type: "text/plain".to_string(),
                name: Some("note.txt".to_string()),
                data_base64: "aGVsbG8=".to_string(),
            }],
            provider: None,
            model: None,
            queue_mode: Some("next_tool_call".to_string()),
        })
        .await;

    let count = handle
        .admit_text_only_queued_turns(|_| panic!("attachment turn must not admit mid-turn"))
        .await
        .unwrap();

    assert_eq!(count, 0);
    assert_eq!(handle.queued.lock().unwrap().len(), 1);
    assert!(handle.queue_path.clone().unwrap().exists());
}
