#!/usr/bin/env bash
set -euo pipefail
git pull origin master
echo "=== Running tests in release mode ==="
cargo test --release

echo "=== Stopping dyson service ==="
systemctl --user stop dyson 2>/dev/null || true
systemctl --user disable dyson 2>/dev/null || true
rm -f ~/.config/systemd/user/dyson.service
systemctl --user daemon-reload



echo "=== Installing ==="
# Forward supported flags to dyson init.
# Usage: ./scripts/update.sh --dangerous-no-sandbox --env OPENROUTER_API_KEY=sk-...
init_args=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --env)
            init_args+=("--env" "$2")
            shift 2
            ;;
        --dangerous-no-sandbox)
            init_args+=("--dangerous-no-sandbox")
            shift
            ;;
        *)
            shift
            ;;
    esac
done
./target/release/dyson init --noinput --daemonize "${init_args[@]+"${init_args[@]}"}"

echo "=== Done ==="
systemctl --user status dyson
