// ===========================================================================
// Framework/language stack specialists for the Hunt stage.
//
// `stack_specialists` turns the deterministic stack detection into one
// specialist hunter per (top-2) language + one per detected framework, each
// briefed with only its own reference material so detection-driven coverage
// no longer bloats (or gets truncated out of) the shared hunt prompt.
//
// `class_provably_inapplicable` is the conservative pruning gate that drops
// only classes that are unambiguously moot for the detected stack —
// everything behavior-dependent (auth, injection, crypto, ssrf, ...) always
// runs, so a detection miss can never create a coverage blind spot.
// ===========================================================================

use super::types::{SecurityCheckpoint, TaskStatus};
use crate::skill::subagent::repo_detect::{self, Detection};

/// A framework/language briefing turned into its own specialist hunter.
/// Carries only its own reference material, so detection-driven coverage no
/// longer bloats (or gets truncated out of) any shared prompt.
pub(super) struct StackSpecialist {
    pub task_id: String,
    pub label: String,
    pub scope_hint: String,
    pub briefing: String,
}

/// Build the framework/language specialists for a detected stack: the top two
/// languages plus every framework detection found (already scoped to selected
/// languages by `detect_repo`).
pub(super) fn stack_specialists(detection: &Detection) -> Vec<StackSpecialist> {
    let mut out = Vec::new();
    for lang in detection.languages.iter().take(2) {
        let (name, content) = repo_detect::language_briefing(*lang);
        out.push(make_stack_specialist(name, content));
    }
    for fw in &detection.frameworks {
        let (name, content) = repo_detect::framework_briefing(*fw);
        out.push(make_stack_specialist(name, content));
    }
    out
}

pub(super) fn make_stack_specialist(name: &str, content: &str) -> StackSpecialist {
    StackSpecialist {
        task_id: format!("sheet-{}", name.replace('/', "-")),
        label: name.to_string(),
        scope_hint: format!("{name} idiomatic security sinks and footguns"),
        briefing: format!(
            "## Your specialization\n\nYou are the dedicated **{name}** specialist hunter for this \
             run. Hunt this stack's idiomatic security sinks, footguns, and misuse patterns; the \
             cross-cutting vulnerability-class specialists cover the rest. Confirm with `ast_query` / \
             `taint_trace` / `attack_surface_analyzer`; report a candidate only with a source, a \
             sink/decision, and a reachability claim backed by a real tool call.\n\nReference for \
             {name}:\n\n{content}\n"
        ),
    }
}

/// Conservative deterministic pruning: only classes that are unambiguously
/// moot for the detected stack are dropped.  Everything behavior-dependent
/// (auth, injection, crypto, ssrf, ...) always runs, so a detection miss can
/// never create a coverage blind spot.
pub(super) fn class_provably_inapplicable(class_id: &str, detection: &Detection) -> bool {
    // No recognized manifests/lockfiles anywhere → nothing to scan for known
    // CVEs or supply-chain risk.  This is the one fully safe signal; expand
    // here (CI, container, frontend) once detection grows recursive presence
    // checks that won't false-prune nested artifacts.
    class_id == "dependency_supply_chain" && detection.languages.is_empty()
}

pub(super) fn prune_inapplicable_class_tasks(
    checkpoint: &mut SecurityCheckpoint,
    detection: &Detection,
) {
    let pending = std::mem::take(&mut checkpoint.pending_tasks);
    for mut task in pending {
        if task.status == TaskStatus::Pending
            && class_provably_inapplicable(&task.attack_class, detection)
        {
            task.status = TaskStatus::Completed;
            if task.rationale.trim().is_empty() {
                task.rationale = "skipped: provably inapplicable to detected stack".to_string();
            }
            checkpoint.completed_tasks.push(task);
        } else {
            checkpoint.pending_tasks.push(task);
        }
    }
}
