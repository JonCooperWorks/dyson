#!/usr/bin/env python3
"""Provision template bootstrap trust without plaintext files or secret argv.

The stable plaintext lives only in Swarm's sealed system store. Only its Argon2
hash enters the image. Keep the hash with deployment backups; do not rotate one
half while templates built with the other half remain available.
"""
import argparse
import os
from pathlib import Path
import secrets
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument('--dyson', required=True)
parser.add_argument('--hash-file', type=Path, required=True)
parser.add_argument('--config', required=True)
args = parser.parse_args()
name = 'dyson.bootstrap_token'
ctl = ['sudo', '-n', '-u', 'dyson-swarm', 'swarmctl', '--config', args.config, 'secrets']
listed = subprocess.run(ctl + ['system-list'], capture_output=True, text=True)
if listed.returncode:
    raise SystemExit('Cannot inspect bootstrap provisioning; command output withheld.')
exists = name in listed.stdout
if args.hash_file.exists():
    if not args.hash_file.read_text().strip().startswith('$argon2id$') or not exists:
        raise SystemExit('Bootstrap hash and sealed secret are inconsistent; restore the matching pair.')
    print('Dyson bootstrap trust already provisioned.')
else:
    if exists:
        raise SystemExit('Sealed bootstrap secret exists but hash is missing; restore its deployment hash backup.')
    token = secrets.token_hex(32)
    hashed = subprocess.run([args.dyson, 'hash-bearer', '--stdin'], input=token, capture_output=True, text=True)
    if hashed.returncode or not hashed.stdout.strip().startswith('$argon2id$'):
        raise SystemExit('Bootstrap hashing failed; command output withheld.')
    args.hash_file.parent.mkdir(parents=True, exist_ok=True)
    # Keep the nonsecret hash recoverable if sealing succeeds but rename fails.
    pending = args.hash_file.with_suffix('.pending')
    fd = os.open(pending, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as output:
        output.write(hashed.stdout.strip() + '\n')
        output.flush()
        os.fsync(output.fileno())
    sealed = subprocess.run(ctl + ['system-set', '--stdin', name], input=token, capture_output=True, text=True)
    del token
    if sealed.returncode:
        pending.unlink()
        raise SystemExit('Bootstrap sealing failed; command output withheld.')
    pending.replace(args.hash_file)
    print('Dyson bootstrap trust provisioned; only its hash will enter the template.')
