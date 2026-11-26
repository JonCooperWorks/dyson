//! Monotonic chat / file / artefact id recovery from on-disk state.

use super::*;

/// Scan the chat directory (files + archives + artefact metadata) for
/// the highest `c-NNNN` ever used.  Ensures a new chat id never reuses
/// a slot that another record still points at — otherwise the empty
/// new chat would surface orphan artefacts filtered by the old id.
pub(crate) fn max_chat_id_n(data_dir: &std::path::Path, artefacts: &ArtefactStore) -> u64 {
    let mut max_n: u64 = 0;
    if let Ok(iter) = std::fs::read_dir(data_dir) {
        for entry in iter.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && let Some(n) = chat_id_n(name)
            {
                max_n = max_n.max(n);
            }
        }
    }
    // Artefacts retain the owning chat_id even when the chat file has
    // been purged — rotation leaves them orphaned on disk.  Walk the
    // in-memory index (already hydrated from disk) for c-NNNN hits.
    for entry in artefacts.items.values() {
        if let Some(n) = chat_id_n(&entry.chat_id) {
            max_n = max_n.max(n);
        }
    }
    max_n
}

pub(super) fn chat_id_n(name: &str) -> Option<u64> {
    let stem = name.strip_prefix("c-")?;
    let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

pub(super) fn prefixed_numeric_id(stem: &str, prefix: char) -> Option<u64> {
    stem.strip_prefix(prefix)
        .and_then(|rest| rest.parse::<u64>().ok())
}

pub(super) fn file_id_from_name(name: &str) -> Option<u64> {
    let stem = name
        .strip_suffix(".meta.json")
        .or_else(|| name.strip_suffix(".bin"))?;
    prefixed_numeric_id(stem, 'f')
}

pub(super) fn artefact_id_from_name(name: &str) -> Option<u64> {
    let stem = name
        .strip_suffix(".meta.json")
        .or_else(|| name.strip_suffix(".body"))?;
    prefixed_numeric_id(stem, 'a')
}

pub(super) fn advance_counter(counter: &std::sync::atomic::AtomicU64, next: u64) {
    let mut current = counter.load(std::sync::atomic::Ordering::Relaxed);
    while current < next {
        match counter.compare_exchange(
            current,
            next,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}
