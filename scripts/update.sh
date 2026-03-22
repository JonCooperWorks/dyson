#!/usr/bin/env bash
set -euo pipefail
git pull origin master
echo "=== Running tests ==="
cargo test

echo "=== Stopping dyson service ==="
systemctl --user stop dyson 2>/dev/null || true
systemctl --user disable dyson 2>/dev/null || true
rm -f ~/.config/systemd/user/dyson.service
systemctl --user daemon-reload

echo "=== Building dyson ==="
cargo build --release

echo "=== Installing ==="
# Pass any extra args (e.g. --env KEY=VALUE) to dyson init.
# Usage: ./scripts/update.sh --env OPENROUTER_API_KEY=sk-... --env RUST_LOG=debug
./target/release/dyson init --noinput --daemonize "$@"

echo "=== Done ==="
systemctl --user status dyson
