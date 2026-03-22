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
# Extract --env KEY=VALUE pairs and pass them to dyson init.
# Usage: ./scripts/update.sh --env OPENROUTER_API_KEY=sk-... --env RUST_LOG=debug
env_args=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --env)
            env_args+=("--env" "$2")
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done
./target/release/dyson init --noinput --daemonize "${env_args[@]+"${env_args[@]}"}"

echo "=== Done ==="
systemctl --user status dyson
