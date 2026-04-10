//! Routing logic — pick a node to run a task.
//!
//! v1 rules:
//!
//! 1. Filter to `NodeStatus::Idle` nodes.
//! 2. Drop nodes missing any required capability.
//! 3. Drop nodes that don't satisfy GPU or RAM constraints.
//! 4. Prefer the one with the most free RAM (ties: lowest node_id lex).
//! 5. If nothing survives, return `None`.
//!
//! No priority queues, no preemption, no affinity.

use dyson_swarm_protocol::types::NodeStatus;

use crate::registry::{NodeEntry, NodeId, NodeRegistry};

/// Constraints the caller provides via `swarm_dispatch`.
#[derive(Debug, Clone, Default)]
pub struct RoutingConstraints {
    pub needs_gpu: bool,
    pub needs_capability: Option<String>,
    pub min_ram_gb: Option<u64>,
}

/// Check whether a single entry satisfies a set of constraints.
///
/// Pulled out of `select_node` so it can be unit-tested in isolation.
pub fn entry_matches(entry: &NodeEntry, constraints: &RoutingConstraints) -> bool {
    if !matches!(entry.status, NodeStatus::Idle) {
        return false;
    }

    if let Some(cap) = &constraints.needs_capability
        && !entry.manifest.capabilities.iter().any(|c| c == cap)
    {
        return false;
    }

    if constraints.needs_gpu && entry.manifest.hardware.gpus.is_empty() {
        return false;
    }

    if let Some(min_gb) = constraints.min_ram_gb {
        let min_bytes = min_gb.saturating_mul(1024 * 1024 * 1024);
        if entry.manifest.hardware.ram_bytes < min_bytes {
            return false;
        }
    }

    true
}

/// Pick a node from the registry for the given constraints.
pub async fn select_node(
    registry: &NodeRegistry,
    constraints: &RoutingConstraints,
) -> Option<NodeId> {
    registry
        .with_entries(|entries| {
            let mut candidates: Vec<&NodeEntry> = entries
                .values()
                .filter(|e| entry_matches(e, constraints))
                .collect();

            // Sort by (most free RAM first, lowest node_id lex for ties).
            candidates.sort_by(|a, b| {
                b.manifest
                    .hardware
                    .ram_bytes
                    .cmp(&a.manifest.hardware.ram_bytes)
                    .then_with(|| a.node_id.cmp(&b.node_id))
            });

            candidates.first().map(|e| e.node_id.clone())
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dyson_swarm_protocol::types::{
        CpuInfo, GpuInfo, HardwareInfo, NodeManifest, NodeStatus,
    };

    fn entry(id: &str, ram_gb: u64, gpus: usize, caps: &[&str], status: NodeStatus) -> NodeEntry {
        NodeEntry {
            node_id: id.into(),
            token: format!("tok-{id}"),
            manifest: NodeManifest {
                node_name: id.into(),
                os: "linux".into(),
                hardware: HardwareInfo {
                    cpus: vec![CpuInfo {
                        model: "test".into(),
                        cores: 4,
                    }],
                    gpus: (0..gpus)
                        .map(|_| GpuInfo {
                            model: "test gpu".into(),
                            vram_bytes: 8 * 1024 * 1024 * 1024,
                            driver: "test".into(),
                        })
                        .collect(),
                    ram_bytes: ram_gb * 1024 * 1024 * 1024,
                    disk_free_bytes: 0,
                },
                capabilities: caps.iter().map(|s| s.to_string()).collect(),
                status: status.clone(),
            },
            status,
            last_heartbeat: std::time::Instant::now(),
            sse_tx: None,
        }
    }

    #[test]
    fn matches_idle_node_with_no_constraints() {
        let e = entry("a", 8, 0, &[], NodeStatus::Idle);
        assert!(entry_matches(&e, &RoutingConstraints::default()));
    }

    #[test]
    fn rejects_busy_node() {
        let e = entry(
            "a",
            8,
            0,
            &[],
            NodeStatus::Busy {
                task_id: "t".into(),
            },
        );
        assert!(!entry_matches(&e, &RoutingConstraints::default()));
    }

    #[test]
    fn rejects_draining_node() {
        let e = entry("a", 8, 0, &[], NodeStatus::Draining);
        assert!(!entry_matches(&e, &RoutingConstraints::default()));
    }

    #[test]
    fn rejects_missing_capability() {
        let e = entry("a", 8, 0, &["bash"], NodeStatus::Idle);
        let c = RoutingConstraints {
            needs_capability: Some("web_search".into()),
            ..Default::default()
        };
        assert!(!entry_matches(&e, &c));
    }

    #[test]
    fn accepts_required_capability() {
        let e = entry("a", 8, 0, &["bash", "web_search"], NodeStatus::Idle);
        let c = RoutingConstraints {
            needs_capability: Some("web_search".into()),
            ..Default::default()
        };
        assert!(entry_matches(&e, &c));
    }

    #[test]
    fn needs_gpu_filters_cpu_only_nodes() {
        let e = entry("a", 8, 0, &[], NodeStatus::Idle);
        let c = RoutingConstraints {
            needs_gpu: true,
            ..Default::default()
        };
        assert!(!entry_matches(&e, &c));
    }

    #[test]
    fn min_ram_gb_filters_small_nodes() {
        let e = entry("a", 8, 0, &[], NodeStatus::Idle);
        let c = RoutingConstraints {
            min_ram_gb: Some(16),
            ..Default::default()
        };
        assert!(!entry_matches(&e, &c));
    }

    #[tokio::test]
    async fn select_node_returns_none_on_empty_registry() {
        let reg = NodeRegistry::new();
        let pick = select_node(&reg, &RoutingConstraints::default()).await;
        assert!(pick.is_none());
    }

    #[tokio::test]
    async fn select_node_prefers_most_ram() {
        let reg = NodeRegistry::new();
        let mut big = entry("big", 64, 0, &[], NodeStatus::Idle);
        let mut small = entry("small", 8, 0, &[], NodeStatus::Idle);
        big.node_id = "big".into();
        small.node_id = "small".into();

        {
            let mut inner = reg.inner_for_test().await;
            inner.by_id.insert("big".into(), big);
            inner.by_id.insert("small".into(), small);
        }

        let pick = select_node(&reg, &RoutingConstraints::default()).await;
        assert_eq!(pick.as_deref(), Some("big"));
    }

    #[tokio::test]
    async fn select_node_tie_breaks_by_node_id() {
        let reg = NodeRegistry::new();
        let a = entry("a", 16, 0, &[], NodeStatus::Idle);
        let b = entry("b", 16, 0, &[], NodeStatus::Idle);
        {
            let mut inner = reg.inner_for_test().await;
            inner.by_id.insert("b".into(), b);
            inner.by_id.insert("a".into(), a);
        }
        let pick = select_node(&reg, &RoutingConstraints::default()).await;
        assert_eq!(pick.as_deref(), Some("a"));
    }
}
