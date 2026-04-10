#!/usr/bin/env bash
#
# Update, build, and (re)start the Dyson swarm hub.
#
# Everything the `swarm` binary accepts is configurable from the CLI,
# as is the git remote/branch to pull, the location of the hub key,
# whether to run the test suite, and whether to run the swarm in the
# foreground or install it as a systemd --user service.
#
# Usage:
#   ./scripts/update-swarm.sh [options]
#
# Options:
#   --remote <name>              Git remote to pull from (default: origin)
#   --branch <name>              Git branch to pull      (default: master)
#   --no-pull                    Skip the git pull step
#   --no-tests                   Skip `cargo test`
#   --no-build                   Skip `cargo build --release`
#   --profile <debug|release>    Cargo profile to build/run (default: release)
#
#   --bind <host:port>           Address for the hub to listen on
#                                (default: 127.0.0.1:8080)
#   --data-dir <path>            Directory for hub.key and blobs/
#                                (default: ./hub-data)
#   --heartbeat-timeout-secs <n> Reap nodes idle for longer than N seconds
#                                (default: 90)
#   --log-level <filter>         tracing env filter (default: info)
#
#   --key-path <path>            Override path to hub.key
#                                (default: <data-dir>/hub.key)
#   --generate-key               Generate hub.key if missing (default: on)
#   --no-generate-key            Fail instead of generating a missing key
#
#   --foreground                 Run the swarm in the foreground instead
#                                of installing a systemd --user service
#   --service-name <name>        systemd unit name          (default: swarm)
#   --env KEY=VALUE              Extra env var for the service
#                                (repeatable)
#
#   -h, --help                   Show this help and exit
#
# Examples:
#   ./scripts/update-swarm.sh --bind 0.0.0.0:8080 --data-dir /var/lib/swarm
#   ./scripts/update-swarm.sh --foreground --log-level debug
#   ./scripts/update-swarm.sh --branch main --env RUST_BACKTRACE=1

set -euo pipefail

# ------------------------------------------------------------------ defaults --
REMOTE="origin"
BRANCH="master"
DO_PULL=1
DO_TESTS=1
DO_BUILD=1
PROFILE="release"

BIND="127.0.0.1:8080"
DATA_DIR="./hub-data"
HEARTBEAT_TIMEOUT_SECS="90"
LOG_LEVEL="info"

KEY_PATH=""
GENERATE_KEY=1

FOREGROUND=0
SERVICE_NAME="swarm"
ENV_VARS=()

usage() {
    sed -n '2,48p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

# --------------------------------------------------------------- arg parsing --
while [[ $# -gt 0 ]]; do
    case "$1" in
        --remote)                REMOTE="$2"; shift 2 ;;
        --branch)                BRANCH="$2"; shift 2 ;;
        --no-pull)               DO_PULL=0; shift ;;
        --no-tests)              DO_TESTS=0; shift ;;
        --no-build)              DO_BUILD=0; shift ;;
        --profile)               PROFILE="$2"; shift 2 ;;

        --bind)                  BIND="$2"; shift 2 ;;
        --data-dir)              DATA_DIR="$2"; shift 2 ;;
        --heartbeat-timeout-secs) HEARTBEAT_TIMEOUT_SECS="$2"; shift 2 ;;
        --log-level)             LOG_LEVEL="$2"; shift 2 ;;

        --key-path)              KEY_PATH="$2"; shift 2 ;;
        --generate-key)          GENERATE_KEY=1; shift ;;
        --no-generate-key)       GENERATE_KEY=0; shift ;;

        --foreground)            FOREGROUND=1; shift ;;
        --service-name)          SERVICE_NAME="$2"; shift 2 ;;
        --env)                   ENV_VARS+=("$2"); shift 2 ;;

        -h|--help)               usage 0 ;;
        *)
            echo "unknown option: $1" >&2
            usage 1
            ;;
    esac
done

case "$PROFILE" in
    debug|release) ;;
    *) echo "invalid --profile: $PROFILE (expected 'debug' or 'release')" >&2; exit 1 ;;
esac

# Resolve repo root so all paths are deterministic regardless of cwd.
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." &>/dev/null && pwd)"
cd "$REPO_ROOT"

# Normalize data dir and key path to absolute so systemd sees real paths.
case "$DATA_DIR" in
    /*) ;;
    *)  DATA_DIR="$REPO_ROOT/${DATA_DIR#./}" ;;
esac
if [[ -z "$KEY_PATH" ]]; then
    KEY_PATH="$DATA_DIR/hub.key"
fi

# --------------------------------------------------------------------- pull --
if [[ "$DO_PULL" -eq 1 ]]; then
    echo "=== Pulling $REMOTE/$BRANCH ==="
    git pull "$REMOTE" "$BRANCH"
fi

# -------------------------------------------------------------------- tests --
if [[ "$DO_TESTS" -eq 1 ]]; then
    echo "=== Running tests (swarm) ==="
    cargo test -p swarm
fi

# -------------------------------------------------------------------- build --
build_args=(-p swarm)
if [[ "$PROFILE" == "release" ]]; then
    build_args+=(--release)
fi
if [[ "$DO_BUILD" -eq 1 ]]; then
    echo "=== Building swarm ($PROFILE) ==="
    cargo build "${build_args[@]}"
fi

SWARM_BIN="$REPO_ROOT/target/$PROFILE/swarm"
KEYGEN_BIN="$REPO_ROOT/target/$PROFILE/swarm-keygen"
if [[ ! -x "$SWARM_BIN" ]]; then
    echo "error: $SWARM_BIN not found. Run without --no-build." >&2
    exit 1
fi

# ---------------------------------------------------------------------- key --
mkdir -p "$DATA_DIR"
if [[ ! -f "$KEY_PATH" ]]; then
    if [[ "$GENERATE_KEY" -eq 1 ]]; then
        echo "=== Generating hub key at $KEY_PATH ==="
        "$KEYGEN_BIN" --out "$KEY_PATH"
    else
        echo "error: hub key missing at $KEY_PATH (pass --generate-key to create one)" >&2
        exit 1
    fi
fi

# --------------------------------------------------------- run / install -----
SWARM_ARGS=(
    --bind "$BIND"
    --data-dir "$DATA_DIR"
    --heartbeat-timeout-secs "$HEARTBEAT_TIMEOUT_SECS"
    --log-level "$LOG_LEVEL"
)

if [[ "$FOREGROUND" -eq 1 ]]; then
    echo "=== Running swarm in foreground ==="
    for kv in "${ENV_VARS[@]+"${ENV_VARS[@]}"}"; do
        export "$kv"
    done
    exec "$SWARM_BIN" "${SWARM_ARGS[@]}"
fi

# systemd --user path.
if ! command -v systemctl >/dev/null 2>&1; then
    echo "error: systemctl not found; re-run with --foreground" >&2
    exit 1
fi

UNIT_DIR="$HOME/.config/systemd/user"
UNIT_PATH="$UNIT_DIR/${SERVICE_NAME}.service"
mkdir -p "$UNIT_DIR"

echo "=== Stopping ${SERVICE_NAME} service (if running) ==="
systemctl --user stop "${SERVICE_NAME}.service" 2>/dev/null || true
systemctl --user disable "${SERVICE_NAME}.service" 2>/dev/null || true
rm -f "$UNIT_PATH"
systemctl --user daemon-reload

echo "=== Installing ${SERVICE_NAME}.service ==="

# Quote each ExecStart argument so spaces in paths survive systemd parsing.
quote_arg() {
    printf '"%s"' "${1//\"/\\\"}"
}
EXEC_START="$(quote_arg "$SWARM_BIN")"
for a in "${SWARM_ARGS[@]}"; do
    EXEC_START+=" $(quote_arg "$a")"
done

{
    echo "[Unit]"
    echo "Description=Dyson swarm hub"
    echo "After=network-online.target"
    echo "Wants=network-online.target"
    echo
    echo "[Service]"
    echo "Type=simple"
    echo "WorkingDirectory=$REPO_ROOT"
    for kv in "${ENV_VARS[@]+"${ENV_VARS[@]}"}"; do
        echo "Environment=$kv"
    done
    echo "ExecStart=$EXEC_START"
    echo "Restart=on-failure"
    echo "RestartSec=5"
    echo
    echo "[Install]"
    echo "WantedBy=default.target"
} > "$UNIT_PATH"

systemctl --user daemon-reload
systemctl --user enable "${SERVICE_NAME}.service"
systemctl --user restart "${SERVICE_NAME}.service"

echo "=== Done ==="
systemctl --user status "${SERVICE_NAME}.service" --no-pager || true
