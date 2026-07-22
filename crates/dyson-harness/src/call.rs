/// A fully assembled model-requested tool invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

impl ToolCall {
    /// Construct a call with a process-unique synthetic ID.
    pub fn new(name: impl Into<String>, input: serde_json::Value) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = name.into();
        Self {
            id: format!("call_{name}_{n}"),
            name,
            input,
        }
    }
}
