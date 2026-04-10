//! `swarm-keygen` — generate a new Ed25519 signing keypair for the swarm hub.
//!
//! The hub binary intentionally does not generate its own key: key
//! provisioning is an explicit, one-shot operation.  Run this once
//! before standing up a hub, then point `swarm` at the same path.
//!
//! ```bash
//! swarm-keygen --out ./hub-data/hub.key
//! swarm --data-dir ./hub-data
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use swarm::key::{HubKeyPair, KeyError};

#[derive(Debug, Parser)]
#[command(
    name = "swarm-keygen",
    about = "Generate an Ed25519 signing keypair for the Dyson swarm hub"
)]
struct Args {
    /// Path to write the PKCS#8 keypair to.
    #[arg(long, default_value = "./hub-data/hub.key")]
    out: PathBuf,
}

fn main() -> ExitCode {
    let args = Args::parse();

    match HubKeyPair::generate(&args.out) {
        Ok(key) => {
            println!("Wrote new hub signing key to {}", args.out.display());
            println!();
            println!("Public key (add to each node's dyson.json):");
            println!("    {}", key.public_key_config());
            println!();
            println!("Start the hub with:");
            println!("    swarm --data-dir {}", parent_display(&args.out));
            ExitCode::SUCCESS
        }
        Err(KeyError::AlreadyExists(p)) => {
            eprintln!("error: a key already exists at {p}");
            eprintln!("refusing to overwrite — delete it first if you really mean to rotate");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Show the parent directory of a key path, or "." if it has none.
fn parent_display(path: &std::path::Path) -> String {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".to_string())
}
