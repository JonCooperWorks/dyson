#!/usr/bin/env bash
# Managed-Dyson boot wrapper. Provider credentials never enter the VM. Native
# CLI scratch/config lives on tmpfs so even non-secret session artefacts vanish
# with the microVM; durable OAuth state is KMS-sealed by Swarm.
set -euo pipefail

subscription_root=/dev/shm/dyson-subscriptions
codex_root="${subscription_root}/codex"
claude_root="${subscription_root}/claude"

install -d -m 0700 "${subscription_root}"
if [[ "$(stat -f -c %T "${subscription_root}")" != "tmpfs" ]]; then
  echo "refusing to start: subscription credential store is not tmpfs" >&2
  exit 1
fi

install -d -m 0700 "${codex_root}"
install -d -m 0700 "${claude_root}"
if [[ ! -f "${codex_root}/config.toml" ]]; then
  printf '%s\n' 'cli_auth_credentials_store = "file"' > "${codex_root}/config.toml"
  chmod 0600 "${codex_root}/config.toml"
fi

export CODEX_HOME="${codex_root}"
export CLAUDE_CONFIG_DIR="${claude_root}"
exec /usr/local/bin/dyson "$@"
