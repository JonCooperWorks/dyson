//! Process-wide leases cover independent conversations and nested tool execution.
use crate::{
    error::{DysonError, Result},
    tool::{ResourceAccess, ResourceClaim, ToolContext, ToolExecutionPlan},
};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::Notify;
struct Held {
    id: u64,
    claims: Vec<ResourceClaim>,
    quarantine: Option<String>,
}
static HELD: Mutex<Vec<Held>> = Mutex::new(Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(1);
static NOTIFY: OnceLock<Notify> = OnceLock::new();
fn notify() -> &'static Notify {
    NOTIFY.get_or_init(Notify::new)
}
pub(super) fn overlap(a: &str, b: &str) -> bool {
    a == "global:tool-execution"
        || b == "global:tool-execution"
        || a == b
        || (a.starts_with("file:")
            && b.starts_with("file:")
            && (a.strip_prefix(b).is_some_and(|v| v.starts_with('/'))
                || b.strip_prefix(a).is_some_and(|v| v.starts_with('/'))))
}
fn conflict(a: &[ResourceClaim], b: &[ResourceClaim]) -> bool {
    a.iter().any(|x| {
        b.iter().any(|y| {
            overlap(&x.key, &y.key)
                && (x.access == ResourceAccess::Write || y.access == ResourceAccess::Write)
        })
    })
}
fn canonical_claim(mut claim: ResourceClaim) -> ResourceClaim {
    if let Some(path) = claim.key.strip_prefix("file:") {
        let mut parent = std::path::PathBuf::from(path);
        let mut suffix = Vec::new();
        while !parent.exists() {
            let Some(name) = parent.file_name().map(|s| s.to_os_string()) else {
                break;
            };
            suffix.push(name);
            if !parent.pop() {
                break;
            }
        }
        if let Ok(mut resolved) = parent.canonicalize() {
            for part in suffix.into_iter().rev() {
                resolved.push(part);
            }
            claim.key = format!("file:{}", resolved.display());
        }
    }
    claim
}
pub struct Lease {
    pub id: u64,
    quarantine: bool,
    pending: Option<String>,
}
impl Lease {
    pub fn arm(&mut self, key: String) {
        self.pending = Some(key);
    }
    pub fn complete(&mut self) {
        self.pending = None;
    }
    pub fn quarantine(&mut self, key: String) {
        self.quarantine = true;
        if let Some(h) = HELD
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .find(|h| h.id == self.id)
        {
            h.quarantine = Some(key);
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(key) = self.pending.take() {
            self.quarantine(key);
        }
        if !self.quarantine {
            HELD.lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|h| h.id != self.id);
            notify().notify_waiters();
        }
    }
}
pub fn reconcile(key: &str) {
    HELD.lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|h| h.quarantine.as_deref() != Some(key));
    notify().notify_waiters();
}
pub async fn acquire(plan: &ToolExecutionPlan, ctx: &ToolContext) -> Result<Lease> {
    let claims: Vec<_> = plan
        .resources
        .iter()
        .cloned()
        .map(canonical_claim)
        .collect();
    let deadline = ctx
        .harness
        .budget
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remaining_time()?
        .min(std::time::Duration::from_millis(plan.timeout_ms.max(1)));
    let wait = async {
        loop {
            let notified = notify().notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
                let blockers: Vec<_> = held
                    .iter()
                    .filter(|h| {
                        !ctx.harness.ancestors.contains(&h.id)
                            && conflict(&h.claims, &claims)
                            && !(h.quarantine.is_some()
                                && claims.iter().all(|c| c.access == ResourceAccess::Read))
                    })
                    .collect();
                if blockers.iter().any(|h| h.quarantine.is_some()) {
                    return Err(DysonError::Llm(
                        "resource has an unresolved mutation; reconcile before use".into(),
                    ));
                }
                if blockers.is_empty() {
                    let id = NEXT.fetch_add(1, Ordering::Relaxed);
                    held.push(Held {
                        id,
                        claims: claims.clone(),
                        quarantine: None,
                    });
                    return Ok(Lease {
                        id,
                        quarantine: false,
                        pending: None,
                    });
                }
            }
            tokio::select! { _=&mut notified=>{}, _=ctx.cancellation.cancelled()=>return Err(DysonError::Llm("resource lease cancelled".into())) }
        }
    };
    tokio::time::timeout(deadline, wait)
        .await
        .map_err(|_| DysonError::Llm("resource lease deadline exceeded".into()))?
}

static MUTATION_EPOCH: AtomicU64 = AtomicU64::new(0);
static PROCESS_EPOCH: OnceLock<String> = OnceLock::new();
pub fn note_mutation() {
    MUTATION_EPOCH.fetch_add(1, Ordering::SeqCst);
}
fn resource_stamp(key: &str) -> String {
    if let Some(path) = key.strip_prefix("file:") {
        let path = std::path::Path::new(path);
        return match std::fs::metadata(path) {
            Ok(meta) => {
                let stamp = format!("{}:{:?}", meta.len(), meta.modified().ok());
                if meta.is_file() && meta.len() <= 8 * 1024 * 1024 {
                    std::fs::read(path).map_or_else(
                        |_| "unreadable".into(),
                        |bytes| {
                            use sha2::Digest as _;
                            format!("{stamp}:{:x}", sha2::Sha256::digest(bytes))
                        },
                    )
                } else {
                    format!("{stamp}:{}", MUTATION_EPOCH.load(Ordering::SeqCst))
                }
            }
            Err(_) => "missing".into(),
        };
    }
    format!(
        "{}:{}",
        PROCESS_EPOCH.get_or_init(|| super::digest(&format!(
            "{}:{}",
            std::process::id(),
            super::budget::now_ms()
        ))),
        MUTATION_EPOCH.load(Ordering::SeqCst)
    )
}
pub fn fingerprints(claims: &[ResourceClaim]) -> std::collections::BTreeMap<String, String> {
    claims
        .iter()
        .map(|c| (c.key.clone(), resource_stamp(&c.key)))
        .collect()
}
pub fn unchanged(sources: &std::collections::BTreeMap<String, String>) -> bool {
    sources.iter().all(|(k, v)| &resource_stamp(k) == v)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn dropping_an_armed_lease_quarantines_until_reconciled() {
        let ctx = ToolContext::for_test(std::path::Path::new("/tmp"));
        let plan = ToolExecutionPlan::write("quarantine-fixture");
        let mut lease = acquire(&plan, &ctx).await.unwrap();
        lease.arm("quarantine-fixture-key".into());
        drop(lease);
        assert!(acquire(&plan, &ctx).await.is_err());
        reconcile("quarantine-fixture-key");
        assert!(acquire(&plan, &ctx).await.is_ok());
    }
    #[tokio::test]
    async fn child_inherits_parent_lease_without_unlocking_siblings() {
        let mut ctx = ToolContext::for_test(std::path::Path::new("/tmp"));
        let plan = ToolExecutionPlan::write("reentrant-fixture");
        let parent = acquire(&plan, &ctx).await.unwrap();
        ctx.harness.ancestors.push(parent.id);
        let child = acquire(&plan, &ctx).await.unwrap();
        assert_ne!(parent.id, child.id);
    }
}
