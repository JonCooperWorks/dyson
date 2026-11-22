# dyson scripts

## Overview
Operator script that updates, tests, builds and (re)installs the `dyson` agent service from a git checkout of the `dyson` repo, using `systemd --user`.

## Interface
- `scripts/update.sh [--env KEY=VALUE]... [--dangerous-no-sandbox]` ([update.sh]):
  - Pulls `origin master` and runs `cargo test --release`.
  - Removes the existing `dyson` user unit.
  - Runs `./target/release/dyson init --noinput --daemonize` with the forwarded flags. Unknown arguments are silently dropped.

## Invariants
- The script uses `set -euo pipefail` ([update.sh]).

## Side Effects
- Git and build: `git pull`, `cargo test` and `cargo build` in the repo ([update.sh]).
- systemd: `systemctl --user stop/disable`, `daemon-reload`, `enable` and `restart`. `update.sh` deletes `~/.config/systemd/user/dyson.service` ([update.sh]).

## Verification
- Run: `bash -n scripts/update.sh` (from `dyson/`). The script has no test suite. The workspace tests run with `cargo test`.
