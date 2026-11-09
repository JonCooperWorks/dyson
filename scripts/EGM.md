# dyson scripts

## Overview
Operator scripts that update, test, build and (re)install services from a git checkout of the `dyson` repo, using `systemd --user`. `update.sh` handles the `dyson` agent service. `update-swarm.sh` handles a `swarm` hub binary, built with `cargo build -p swarm`.

## Interface
- `scripts/update.sh [--env KEY=VALUE]... [--dangerous-no-sandbox]` ([update.sh]):
  - Pulls `origin master` and runs `cargo test --release`.
  - Removes the existing `dyson` user unit.
  - Runs `./target/release/dyson init --noinput --daemonize` with the forwarded flags. Unknown arguments are silently dropped.
- `scripts/update-swarm.sh [options]` ([update-swarm.sh:arg parsing]):
  - Git and build flags: `--remote`, `--branch`, `--no-pull`, `--no-tests`, `--no-build`, `--profile debug|release`.
  - Hub flags: `--bind` (default `127.0.0.1:8080`), `--data-dir` (default `./hub-data`), `--heartbeat-timeout-secs` (default `90`), `--log-level` (default `info`).
  - Key flags: `--key-path`, `--generate-key` or `--no-generate-key`.
  - Service flags: `--foreground`, `--service-name` (default `swarm`), `--env KEY=VALUE` (repeatable), `-h` or `--help`.

## Invariants
- Both scripts use `set -euo pipefail` ([update.sh], [update-swarm.sh]).
- `update-swarm.sh` validation ([update-swarm.sh]):
  - Exits on an unknown option ([update-swarm.sh:usage 1]) or a `--profile` other than `debug` or `release`.
  - Exits if `target/<profile>/swarm` is not executable.
  - Exits if the hub key is missing and `--no-generate-key` was given.
  - Exits if `systemctl` is absent and `--foreground` was not given.
- `update-swarm.sh` resolves the repo root from the script location. It makes `--data-dir` absolute and defaults the key path to `<data-dir>/hub.key` ([update-swarm.sh:REPO_ROOT]).
- Each `ExecStart` argument in the generated unit is double-quoted so spaces survive systemd parsing ([update-swarm.sh:quote_arg]).
- Gap: `update-swarm.sh` builds and tests `-p swarm`, but `dyson/Cargo.toml` lists no `swarm` workspace member. The members are `dyson`, `dyson-ast`, `dyson-core`, `dyson-dependency-analysis`, `dyson-harness` and `dyson-persistence`. In this checkout that step would fail.

## Side Effects
- Git and build: `git pull`, `cargo test` and `cargo build` in the repo ([update.sh], [update-swarm.sh]).
- systemd: `systemctl --user stop/disable`, `daemon-reload`, `enable` and `restart`. `update.sh` deletes `~/.config/systemd/user/dyson.service`. `update-swarm.sh` writes `~/.config/systemd/user/<service-name>.service` with `Restart=on-failure` ([update.sh], [update-swarm.sh]).
- Filesystem: `update-swarm.sh` creates the data directory and can generate `hub.key` via `target/<profile>/swarm-keygen --out` ([update-swarm.sh:key]).
- Process: `--foreground` exports the `--env` values and `exec`s the swarm binary ([update-swarm.sh:FOREGROUND]).

## Verification
- Run: `bash -n scripts/update.sh && bash -n scripts/update-swarm.sh` (from `dyson/`). The scripts have no test suite. The workspace tests run with `cargo test`.
