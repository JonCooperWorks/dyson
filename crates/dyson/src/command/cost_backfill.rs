use std::path::PathBuf;

use dyson::chat_history::create_chat_history;
use dyson::message_cost_backfill::{CostBackfillOptions, backfill_history};
use dyson::swarm_cost::CostLookupConfig;

pub async fn run(
    config: Option<PathBuf>,
    swarm_url: String,
    bearer: Option<String>,
    dry_run: bool,
) -> dyson::error::Result<()> {
    let config_path = super::resolve_config_path(config);
    let settings = dyson::config::loader::load_settings(config_path.as_deref())?;
    let history = create_chat_history(&settings.chat_history)?;
    let Some(costs) = CostLookupConfig::public_api(&swarm_url, bearer.as_deref()) else {
        return Err(dyson::DysonError::Config(
            "Swarm URL for cost backfill must not be empty".into(),
        ));
    };

    let report =
        backfill_history(history.as_ref(), &costs, CostBackfillOptions { dry_run }).await?;

    println!("messages scanned: {}", report.messages_scanned);
    println!("messages linked: {}", report.messages_linked);
    println!("messages priced: {}", report.messages_priced);
    println!("messages skipped: {}", report.messages_skipped);
    for (reason, count) in report.skip_reasons {
        println!("skip {reason}: {count}");
    }
    Ok(())
}
