/// Whether an invocation only observes a resource or may mutate it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess {
    Read,
    Write,
}

/// One normalized resource claimed by an invocation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResourceClaim {
    pub key: String,
    pub access: ResourceAccess,
}

/// Retry semantics used by execution journals and recovery tooling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Idempotency {
    Safe,
    Keyed,
    Unsafe,
}

/// Scheduling and supervision metadata for one concrete invocation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolExecutionPlan {
    pub resources: Vec<ResourceClaim>,
    pub idempotency: Idempotency,
    pub timeout_ms: u64,
}

impl ToolExecutionPlan {
    pub fn exclusive() -> Self {
        Self {
            resources: vec![ResourceClaim {
                key: "global:tool-execution".to_string(),
                access: ResourceAccess::Write,
            }],
            idempotency: Idempotency::Unsafe,
            timeout_ms: 300_000,
        }
    }

    pub fn read(key: impl Into<String>) -> Self {
        Self {
            resources: vec![ResourceClaim {
                key: key.into(),
                access: ResourceAccess::Read,
            }],
            idempotency: Idempotency::Safe,
            timeout_ms: 120_000,
        }
    }

    pub fn write(key: impl Into<String>) -> Self {
        Self {
            resources: vec![ResourceClaim {
                key: key.into(),
                access: ResourceAccess::Write,
            }],
            idempotency: Idempotency::Unsafe,
            timeout_ms: 120_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conservative_default_is_exclusive_and_never_retryable() {
        let plan = ToolExecutionPlan::exclusive();
        assert_eq!(plan.idempotency, Idempotency::Unsafe);
        assert_eq!(plan.resources.len(), 1);
        assert_eq!(plan.resources[0].access, ResourceAccess::Write);
    }
}
