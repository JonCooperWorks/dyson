//! Durable task contracts and evidence. Independent from Swarm evaluation.
pub mod budget;
mod leases;
use crate::{
    chat_history::ChatHistory,
    error::{DysonError, Result},
    tool::{Tool, ToolContext, ToolExecutionPlan, ToolOutput},
};
pub use leases::{acquire, reconcile as reconcile_lease};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

pub const PROMPT: &str = "For action tasks use task_control plan to record the objective, steps and concrete acceptance criteria before working. Criteria specify a tool and a nonempty expected output substring. Use task_control verify with evidence IDs from real tool results; never claim verified completion without passing criteria. Use status after resumption, update to record remaining steps/blockers, and evidence to retrieve full original results. Configured tests prove only their declared checks, not arbitrary correctness. Changed hypotheses or plans alone are not evidence of progress. Do not replace an unfinished task unless the user changes the objective.";
fn error(s: impl Into<String>) -> DysonError {
    DysonError::Llm(s.into())
}
pub fn digest(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct Criterion {
    pub id: String,
    pub description: String,
    pub tool: String,
    pub contains: String,
    #[serde(default)]
    pub evidence: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub id: String,
    pub tool: String,
    pub input_hash: String,
    pub sha256: String,
    pub content: String,
    pub is_error: bool,
    pub revision: u64,
    #[serde(default)]
    pub sources: BTreeMap<String, String>,
}
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct TaskState {
    pub objective: String,
    pub criteria: Vec<Criterion>,
    pub steps: Vec<String>,
    pub blockers: Vec<String>,
    #[serde(default)]
    pub completed_steps: Vec<String>,
    pub mutation_revision: u64,
    pub evidence: Vec<String>,
    pub observations: BTreeSet<String>,
    pub stagnant_calls: usize,
    pub replans: usize,
    pub revision: u64,
    pub has_mutations: bool,
}
impl TaskState {
    pub fn completion(&self) -> &'static str {
        if !self.criteria.is_empty()
            && self.criteria.iter().all(|c| c.evidence.is_some())
            && self.blockers.is_empty()
        {
            "verified"
        } else if self.has_mutations || !self.criteria.is_empty() {
            "unverified"
        } else {
            "answered"
        }
    }
}
#[derive(Clone)]
struct Storage {
    store: Arc<dyn ChatHistory>,
    chat: String,
}
#[derive(Default)]
struct Data {
    task: TaskState,
    evidence: BTreeMap<String, Evidence>,
    receipts: BTreeMap<String, Receipt>,
    pending: BTreeMap<String, Vec<crate::tool::ResourceClaim>>,
    storage: Option<Storage>,
    load_error: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    input_hash: String,
    output: Option<(String, bool)>,
}
#[derive(Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    task: TaskState,
    budget: budget::BudgetState,
    receipts: BTreeMap<String, Receipt>,
    #[serde(default)]
    pending: BTreeMap<String, Vec<crate::tool::ResourceClaim>>,
}
/// Durable task ledger, evidence and budget shared by all child agents.
#[derive(Clone)]
pub struct TaskRuntime {
    data: Arc<Mutex<Data>>,
    pub budget: Arc<Mutex<budget::BudgetState>>,
    pub ancestors: Vec<u64>,
}
impl Default for TaskRuntime {
    fn default() -> Self {
        Self {
            data: Arc::new(Mutex::new(Data::default())),
            budget: Arc::new(Mutex::new(budget::BudgetState::default())),
            ancestors: vec![],
        }
    }
}
impl TaskRuntime {
    pub fn child(&self) -> Self {
        self.clone()
    }
    pub fn attach(&self, store: Arc<dyn ChatHistory>, chat: String) {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if data
            .storage
            .as_ref()
            .is_some_and(|s| s.chat == chat && Arc::ptr_eq(&s.store, &store))
        {
            return;
        }
        let result = store.load_harness_record(&chat, "task").and_then(|value| {
            if let Some(value) = value {
                let checkpoint: Checkpoint = serde_json::from_value(value)?;
                if checkpoint.version != 1 {
                    return Err(error("unsupported task checkpoint version"));
                }
                data.task = checkpoint.task;
                for criterion in &mut data.task.criteria {
                    criterion.evidence = None;
                }
                data.receipts = checkpoint.receipts;
                data.pending = checkpoint.pending;
                let mut budget = self.budget.lock().unwrap_or_else(|e| e.into_inner());
                let configured = budget.limits.clone();
                *budget = checkpoint.budget;
                budget.limits.restrict(&configured);
            }
            Ok(())
        });
        data.load_error = result.err().map(|e| e.to_string());
        data.storage = Some(Storage { store, chat });
    }
    pub fn resume_user_turn(&self) {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        data.task.stagnant_calls = 0;
    }
    pub fn seed_objective(&self, objective: &str) {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if data.task.objective.is_empty() {
            data.task.objective = objective.into();
        }
    }
    pub fn ensure_loaded(&self) -> Result<()> {
        if let Some(e) = &self
            .data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .load_error
        {
            return Err(error(format!("task recovery failed: {e}")));
        }
        Ok(())
    }
    fn save_locked(&self, data: &Data) -> Result<()> {
        if let Some(storage) = &data.storage {
            let checkpoint = Checkpoint {
                version: 1,
                task: data.task.clone(),
                budget: self
                    .budget
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                receipts: data.receipts.clone(),
                pending: data.pending.clone(),
            };
            storage.store.save_harness_record(
                &storage.chat,
                "task",
                &serde_json::to_value(checkpoint)?,
            )?;
        }
        Ok(())
    }
    pub fn checkpoint(&self) -> Result<()> {
        self.ensure_loaded()?;
        self.save_locked(&self.data.lock().unwrap_or_else(|e| e.into_inner()))
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        Self::refresh_verification(&mut data);
        serde_json::json!({"state":data.task,"completion":data.task.completion(),"budget":*self.budget.lock().unwrap_or_else(|e| e.into_inner())})
    }
    pub fn completion(&self) -> String {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        Self::refresh_verification(&mut data);
        data.task.completion().into()
    }
    fn refresh_verification(data: &mut Data) {
        for index in 0..data.task.criteria.len() {
            if let Some(id) = data.task.criteria[index].evidence.clone() {
                let valid = Self::evidence_locked(data, &id).is_ok_and(|e| {
                    !e.is_error
                        && e.revision == data.task.mutation_revision
                        && leases::unchanged(&e.sources)
                });
                if !valid {
                    data.task.criteria[index].evidence = None;
                }
            }
        }
    }
    pub fn validate_observations(&self, plan: &ToolExecutionPlan) -> Result<()> {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let mut checked = BTreeSet::new();
        for id in data.task.evidence.clone().into_iter().rev() {
            let evidence = Self::evidence_locked(&mut data, &id)?;
            for (key, stamp) in evidence.sources {
                if key.starts_with("file:")
                    && checked.insert(key.clone())
                    && plan.resources.iter().any(|r| leases::overlap(&r.key, &key))
                    && !leases::unchanged(&BTreeMap::from([(key.clone(), stamp)]))
                {
                    return Err(error(format!(
                        "stale observation for {key}; read the current resource before modifying it"
                    )));
                }
            }
        }
        Ok(())
    }
    pub fn mark_pending(&self, operation: &str, plan: &ToolExecutionPlan) -> Result<()> {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        data.pending
            .insert(operation.into(), plan.resources.clone());
        self.save_locked(&data)
    }
    pub fn clear_pending(&self, operation: &str) -> Result<()> {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        data.pending.remove(operation);
        self.save_locked(&data)
    }
    pub fn check_foreign_pending(&self, plan: &ToolExecutionPlan) -> Result<()> {
        let storage = self
            .data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .storage
            .clone();
        let Some(storage) = storage else {
            return Ok(());
        };
        for chat in storage.store.list_harness_chats()? {
            if chat == storage.chat {
                continue;
            }
            let Some(value) = storage.store.load_harness_record(&chat, "task")? else {
                continue;
            };
            let checkpoint: Checkpoint = serde_json::from_value(value)?;
            if checkpoint.pending.is_empty() {
                continue;
            }
            let unresolved = crate::agent::protocol::unresolved_tool_outcomes(
                &storage.store.load_run_events(&chat)?,
            );
            for (operation, claims) in checkpoint.pending {
                if unresolved
                    .iter()
                    .any(|u| format!("{}:{}", u.run_id.0, u.tool_use_id) == operation)
                    && claims.iter().any(|a| {
                        plan.resources
                            .iter()
                            .any(|b| leases::overlap(&a.key, &b.key))
                    })
                {
                    return Err(error(format!(
                        "resource has an unresolved mutation in conversation {chat} ({operation}); reconcile it before writing"
                    )));
                }
            }
        }
        Ok(())
    }
    pub fn prepare_mutation(&self) -> Result<()> {
        leases::note_mutation();
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        data.task.has_mutations = true;
        data.task.mutation_revision += 1;
        for c in &mut data.task.criteria {
            c.evidence = None;
        }
        self.save_locked(&data)
    }
    pub fn record(
        &self,
        name: &str,
        input: &serde_json::Value,
        out: &ToolOutput,
        plan: &ToolExecutionPlan,
    ) -> Result<String> {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let sha = digest(&out.content);
        let id = format!(
            "e-{}",
            digest(&format!(
                "{name}:{}:{sha}:{}",
                input, data.task.mutation_revision
            ))
        );
        let evidence = Evidence {
            id: id.clone(),
            tool: name.into(),
            input_hash: digest(&input.to_string()),
            sha256: sha.clone(),
            content: out.content.clone(),
            is_error: out.is_error,
            revision: data.task.mutation_revision,
            sources: leases::fingerprints(&plan.resources),
        };
        if let Some(storage) = &data.storage {
            storage.store.save_harness_record(
                &storage.chat,
                &id,
                &serde_json::to_value(&evidence)?,
            )?;
        }
        let novel = !out.is_error && data.task.observations.insert(sha);
        data.task.stagnant_calls = if novel {
            0
        } else {
            data.task.stagnant_calls + 1
        };
        data.task.evidence.retain(|existing| existing != &id);
        data.task.evidence.push(id.clone());
        data.evidence.insert(id.clone(), evidence);
        self.save_locked(&data)?;
        Ok(id)
    }
    fn evidence_locked(data: &mut Data, id: &str) -> Result<Evidence> {
        if !data.task.evidence.iter().any(|e| e == id) {
            return Err(error("unknown evidence ID"));
        }
        if let Some(e) = data.evidence.get(id) {
            return Ok(e.clone());
        }
        let storage = data
            .storage
            .as_ref()
            .ok_or_else(|| error("evidence storage unavailable"))?;
        let value = storage
            .store
            .load_harness_record(&storage.chat, id)?
            .ok_or_else(|| error("missing evidence artifact"))?;
        let evidence: Evidence = serde_json::from_value(value)?;
        if evidence.id != id || digest(&evidence.content) != evidence.sha256 {
            return Err(error("evidence integrity check failed"));
        }
        data.evidence.insert(id.into(), evidence.clone());
        Ok(evidence)
    }
    pub fn stagnant_calls(&self) -> usize {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .task
            .stagnant_calls
    }
    pub fn resume_summary(&self) -> serde_json::Value {
        let data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let t = &data.task;
        serde_json::json!({"objective":t.objective,"criteria":t.criteria,"steps":t.steps,"completed_steps":t.completed_steps,"blockers":t.blockers,"completion":t.completion(),"recent_evidence":t.evidence.iter().rev().take(8).collect::<Vec<_>>()})
    }
    pub fn receipt_pending(&self, key: &str, input_hash: &str) -> Result<bool> {
        let data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = data.receipts.get(key) {
            if r.input_hash != input_hash {
                return Err(error("idempotency key reused with different arguments"));
            }
            return Ok(r.output.is_none());
        }
        Ok(false)
    }
    pub fn receipt(&self, key: &str, input_hash: &str) -> Result<Option<ToolOutput>> {
        let data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        match data.receipts.get(key) {
            Some(r) if r.input_hash != input_hash => Err(error("idempotency key reused with different arguments")),
            Some(r) => r.output.clone().map(|(content,is_error)| Some(if is_error { ToolOutput::error(content) } else { ToolOutput::success(content) })).ok_or_else(|| error("keyed operation outcome unknown; reconcile using provider lookup before retrying")),
            None => Ok(None),
        }
    }
    pub fn receipt_write(
        &self,
        key: &str,
        input_hash: &str,
        output: Option<ToolOutput>,
    ) -> Result<()> {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        data.receipts.insert(
            key.into(),
            Receipt {
                input_hash: input_hash.into(),
                output: output.map(|out| (out.content, out.is_error)),
            },
        );
        self.save_locked(&data)
    }
}

pub struct TaskTool;
#[async_trait::async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task_control"
    }
    fn description(&self) -> &str {
        "Record/resume task contracts, steps and blockers; verify acceptance checks against real evidence; retrieve complete original tool output by ID. Does not run evaluations."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","required":["action"],"properties":{"action":{"type":"string","enum":["plan","status","update","verify","evidence"]},"objective":{"type":"string"},"criteria":{"type":"array","items":{"type":"object"}},"steps":{"type":"array","items":{"type":"string"}},"completed_steps":{"type":"array","items":{"type":"string"}},"blockers":{"type":"array","items":{"type":"string"}},"criterion":{"type":"string"},"evidence_id":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":16000},"replace":{"type":"boolean"}}})
    }
    fn execution_plan(&self, _: &serde_json::Value, ctx: &ToolContext) -> ToolExecutionPlan {
        ToolExecutionPlan::read(format!(
            "task:{}",
            ctx.current_chat_id.as_deref().unwrap_or("local")
        ))
    }
    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let runtime = &ctx.harness;
        runtime.ensure_loaded()?;
        let action = input["action"].as_str().unwrap_or_default();
        if action == "status" {
            return Ok(ToolOutput::success(runtime.snapshot().to_string()));
        }
        let mut data = runtime.data.lock().unwrap_or_else(|e| e.into_inner());
        match action {
            "plan" => {
                let objective = input["objective"].as_str().unwrap_or_default().trim();
                if objective.is_empty() {
                    return Err(error("objective required"));
                }
                if !data.task.criteria.is_empty()
                    && data.task.completion() != "verified"
                    && !input["replace"].as_bool().unwrap_or(false)
                {
                    return Err(error(
                        "unfinished task exists; update it, or explicitly replace after the user changes objective",
                    ));
                }
                let mut criteria: Vec<Criterion> =
                    serde_json::from_value(input["criteria"].clone())?;
                let mut ids = BTreeSet::new();
                if criteria.is_empty()
                    || criteria.iter().any(|c| {
                        c.id.trim().is_empty()
                            || c.tool.trim().is_empty()
                            || c.contains.trim().is_empty()
                            || !ids.insert(c.id.clone())
                    })
                {
                    return Err(error(
                        "criteria need unique IDs, tool names and nonempty expected output",
                    ));
                }
                for c in &mut criteria {
                    c.evidence = None;
                }
                data.task.objective = objective.into();
                data.task.criteria = criteria;
                data.task.blockers.clear();
                data.task.revision += 1;
            }
            "verify" => {
                let id = input["evidence_id"].as_str().unwrap_or_default();
                let e = TaskRuntime::evidence_locked(&mut data, id)?;
                let revision = data.task.mutation_revision;
                let c = data
                    .task
                    .criteria
                    .iter_mut()
                    .find(|c| Some(c.id.as_str()) == input["criterion"].as_str())
                    .ok_or_else(|| error("unknown criterion"))?;
                if e.is_error
                    || !leases::unchanged(&e.sources)
                    || e.revision != revision
                    || e.tool != c.tool
                    || !e.content.contains(&c.contains)
                {
                    return Err(error(
                        "evidence is failed, stale, or does not satisfy the declared check",
                    ));
                }
                c.evidence = Some(id.into());
                data.task.stagnant_calls = 0;
                data.task.revision += 1;
            }
            "update" => {
                data.task.replans += 1;
                data.task.revision += 1;
            }
            "evidence" => {
                let e = TaskRuntime::evidence_locked(
                    &mut data,
                    input["evidence_id"].as_str().unwrap_or_default(),
                )?;
                let offset = input["offset"].as_u64().unwrap_or(0) as usize;
                let limit = input["limit"].as_u64().unwrap_or(8000).min(16000) as usize;
                let content: String = e.content.chars().skip(offset).take(limit).collect();
                return Ok(ToolOutput::success(serde_json::json!({"id":e.id,"sha256":e.sha256,"tool":e.tool,"is_error":e.is_error,"offset":offset,"total_chars":e.content.chars().count(),"content":content}).to_string()));
            }
            _ => return Err(error("unknown task action")),
        }
        if let Some(v) = input.get("completed_steps") {
            data.task.completed_steps = serde_json::from_value(v.clone())?;
        }
        if let Some(v) = input.get("steps") {
            data.task.steps = serde_json::from_value(v.clone())?;
        }
        if let Some(v) = input.get("blockers") {
            data.task.blockers = serde_json::from_value(v.clone())?;
        }
        runtime.save_locked(&data)?;
        drop(data);
        Ok(ToolOutput::success(runtime.snapshot().to_string()))
    }
}
