//! Framework/language stack specialists for the Hunt stage.
//!
//! `stack_specialists` turns the deterministic stack detection into one
//! specialist hunter per (top-2) language + one per detected framework, each
//! briefed with only its own reference material so detection-driven coverage
//! no longer bloats (or gets truncated out of) the shared hunt prompt.
//!
//! `class_provably_inapplicable` is the conservative pruning gate that drops
//! only classes that are unambiguously moot for the detected stack —
//! everything behavior-dependent (auth, injection, crypto, ssrf, ...) always
//! runs, so a detection miss can never create a coverage blind spot.

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

#[cfg(test)]
mod tests {
    use super::super::types::{ModelMetadata, SecurityTask, TargetRef};
    use super::*;
    use crate::skill::subagent::repo_detect::Language;

    fn cp_with_pending(tasks: Vec<SecurityTask>) -> SecurityCheckpoint {
        let mut cp = SecurityCheckpoint::new(
            "run".into(),
            TargetRef {
                repo_path: "/repo".into(),
                git_ref: None,
            },
            "scope".into(),
            ModelMetadata {
                provider: "p".into(),
                model: "m".into(),
            },
            0,
        );
        cp.pending_tasks = tasks;
        cp
    }

    #[test]
    fn supply_chain_is_inapplicable_when_no_languages_detected() {
        assert!(
            class_provably_inapplicable("dependency_supply_chain", &Detection::default()),
            "no detected languages means no manifests to scan"
        );
    }

    #[test]
    fn supply_chain_is_applicable_when_a_language_is_detected() {
        let detection = Detection {
            languages: vec![Language::Rust],
            frameworks: vec![],
        };
        assert!(
            !class_provably_inapplicable("dependency_supply_chain", &detection),
            "Rust detection means cargo manifests are present; never prune"
        );
    }

    #[test]
    fn behavior_dependent_class_never_pruned() {
        // auth_authorization is intrinsically behavior-dependent; even an
        // empty detection must never prune it — that would create a
        // silent coverage blind spot.
        assert!(!class_provably_inapplicable(
            "auth_authorization",
            &Detection::default()
        ));
    }

    #[test]
    fn prune_moves_provably_inapplicable_pending_to_completed_with_rationale() {
        let mut cp = cp_with_pending(vec![SecurityTask {
            id: "t1".into(),
            attack_class: "dependency_supply_chain".into(),
            scope_hint: "deps".into(),
            status: TaskStatus::Pending,
            rationale: String::new(),
        }]);
        prune_inapplicable_class_tasks(&mut cp, &Detection::default());
        assert!(
            cp.pending_tasks.is_empty(),
            "provably inapplicable task should leave pending"
        );
        assert_eq!(cp.completed_tasks.len(), 1);
        let done = &cp.completed_tasks[0];
        assert_eq!(done.status, TaskStatus::Completed);
        assert!(
            done.rationale.contains("provably inapplicable"),
            "rationale should mention 'provably inapplicable', got {:?}",
            done.rationale
        );
    }

    #[test]
    fn prune_leaves_behavior_dependent_classes_in_pending() {
        let mut cp = cp_with_pending(vec![
            SecurityTask {
                id: "t1".into(),
                attack_class: "auth_authorization".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            },
            SecurityTask {
                id: "t2".into(),
                attack_class: "injection_unsafe_execution".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            },
            SecurityTask {
                id: "t3".into(),
                attack_class: "crypto_randomness".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            },
        ]);
        prune_inapplicable_class_tasks(&mut cp, &Detection::default());
        assert_eq!(
            cp.pending_tasks.len(),
            3,
            "behavior-dependent classes must remain pending; got {:?}",
            cp.pending_tasks
        );
        assert!(
            cp.completed_tasks.is_empty(),
            "no behavior-dependent task should be marked completed"
        );
    }
}
