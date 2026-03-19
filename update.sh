#!/usr/bin/env bash
set -euo pipefail

echo "=== Stopping dyson service ==="
systemctl --user stop dyson 2>/dev/null || true
systemctl --user disable dyson 2>/dev/null || true
rm -f ~/.config/systemd/user/dyson.service
systemctl --user daemon-reload

echo "=== Building dyson ==="
cargo build --release

echo "=== Installing ==="
dyson init --noinput --daemonize

echo "=== Done ==="
systemctl --user status dyson
