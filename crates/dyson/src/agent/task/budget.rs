//! Integer accounting with reservations shared by every child of a task.
use crate::error::{DysonError, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_cost_microusd: Option<u64>,
    pub max_elapsed_ms: Option<u64>,
    /// USD per million tokens expressed in micro-USD. Never inferred from a model name.
    pub prices: BTreeMap<String, Price>,
}
impl Limits {
    pub fn restrict(&mut self, configured: &Self) {
        fn tighter(old: Option<u64>, new: Option<u64>) -> Option<u64> {
            match (old, new) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        }
        self.max_input_tokens = tighter(self.max_input_tokens, configured.max_input_tokens);
        self.max_output_tokens = tighter(self.max_output_tokens, configured.max_output_tokens);
        self.max_cost_microusd = tighter(self.max_cost_microusd, configured.max_cost_microusd);
        self.max_elapsed_ms = tighter(self.max_elapsed_ms, configured.max_elapsed_ms);
        self.prices.extend(configured.prices.clone());
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Price {
    pub input_microusd_per_million: u64,
    pub output_microusd_per_million: u64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetState {
    pub limits: Limits,
    pub started_ms: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_microusd: u64,
    pub reservations: u64,
}
fn cost(price: u64, tokens: u64) -> u64 {
    (u128::from(price) * u128::from(tokens))
        .div_ceil(1_000_000)
        .min(u128::from(u64::MAX)) as u64
}
impl BudgetState {
    pub fn remaining_time(&self) -> Result<Duration> {
        if let Some(limit) = self.limits.max_elapsed_ms {
            let elapsed = now_ms().saturating_sub(self.started_ms.unwrap_or_else(now_ms));
            if elapsed >= limit {
                return Err(DysonError::Llm("task elapsed-time budget exhausted".into()));
            }
            return Ok(Duration::from_millis(limit - elapsed));
        }
        Ok(Duration::from_secs(365 * 24 * 60 * 60))
    }
    pub fn exhausted(&self) -> bool {
        self.remaining_time().is_err()
            || self
                .limits
                .max_input_tokens
                .is_some_and(|n| self.input_tokens >= n)
            || self
                .limits
                .max_output_tokens
                .is_some_and(|n| self.output_tokens >= n)
            || self
                .limits
                .max_cost_microusd
                .is_some_and(|n| self.cost_microusd >= n)
    }
}
/// On cancellation or indeterminate failure, the reservation remains charged.
/// Successful calls replace the reservation with observed usage.
pub struct Reservation {
    shared: Arc<Mutex<BudgetState>>,
    input: u64,
    output: u64,
    cost: u64,
    price: Price,
    settled: bool,
}
impl Reservation {
    pub fn reserve(
        shared: Arc<Mutex<BudgetState>>,
        model: &str,
        input: u64,
        requested_output: u32,
    ) -> Result<(Self, u32)> {
        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
        state.started_ms.get_or_insert_with(now_ms);
        state.remaining_time()?;
        let price = state.limits.prices.get(model).cloned().unwrap_or_default();
        if state.limits.max_cost_microusd.is_some() && !state.limits.prices.contains_key(model) {
            return Err(DysonError::Llm(format!(
                "task cost budget requires configured price for {model}"
            )));
        }
        if state
            .limits
            .max_input_tokens
            .is_some_and(|max| input > max.saturating_sub(state.input_tokens))
        {
            return Err(DysonError::Llm("task input-token budget exhausted".into()));
        }
        let mut output = u64::from(requested_output).min(
            state
                .limits
                .max_output_tokens
                .map_or(u64::MAX, |max| max.saturating_sub(state.output_tokens)),
        );
        if let Some(max) = state.limits.max_cost_microusd {
            let remaining = max.saturating_sub(state.cost_microusd);
            let input_cost = cost(price.input_microusd_per_million, input);
            if input_cost > remaining {
                return Err(DysonError::Llm("task cost budget exhausted".into()));
            }
            if price.output_microusd_per_million > 0 {
                output = output.min(
                    ((u128::from(remaining - input_cost) * 1_000_000)
                        / u128::from(price.output_microusd_per_million))
                    .min(u128::from(u64::MAX)) as u64,
                );
            }
        }
        if output == 0 {
            return Err(DysonError::Llm("task output/cost budget exhausted".into()));
        }
        let charge = cost(price.input_microusd_per_million, input)
            .saturating_add(cost(price.output_microusd_per_million, output));
        state.input_tokens = state.input_tokens.saturating_add(input);
        state.output_tokens = state.output_tokens.saturating_add(output);
        state.cost_microusd = state.cost_microusd.saturating_add(charge);
        state.reservations += 1;
        drop(state);
        Ok((
            Self {
                shared,
                input,
                output,
                cost: charge,
                price,
                settled: false,
            },
            output.min(u64::from(u32::MAX)) as u32,
        ))
    }
    pub fn settle(mut self, input: Option<usize>, output: usize) {
        let actual_input = input.map_or(self.input, |v| v as u64);
        let output = output as u64;
        let mut state = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        state.input_tokens = state
            .input_tokens
            .saturating_sub(self.input)
            .saturating_add(actual_input);
        state.output_tokens = state
            .output_tokens
            .saturating_sub(self.output)
            .saturating_add(output);
        state.cost_microusd = state
            .cost_microusd
            .saturating_sub(self.cost)
            .saturating_add(cost(self.price.input_microusd_per_million, actual_input))
            .saturating_add(cost(self.price.output_microusd_per_million, output));
        state.reservations = state.reservations.saturating_sub(1);
        self.settled = true;
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.settled {
            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            s.reservations = s.reservations.saturating_sub(1);
        }
    }
}

tokio::task_local! { pub static ACTIVE: super::TaskRuntime; }
/// Retry decorators cannot know whether a failed pre-stream attempt was billed.
/// Charge its maximum reservation before admitting another attempt.
pub fn charge_uncertain_retry(model: &str, input: u64, output: u32) -> Result<()> {
    ACTIVE
        .try_with(|runtime| {
            let (charge, cap) = Reservation::reserve(runtime.budget.clone(), model, input, output)?;
            drop(charge);
            runtime.checkpoint()?;
            if cap < output {
                return Err(DysonError::Llm(
                    "task budget cannot cover another provider attempt".into(),
                ));
            }
            Ok(())
        })
        .unwrap_or(Ok(()))
}
