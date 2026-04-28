# syntax=docker/dockerfile:1.7
#
# Dyson — packaged as a CubeSandbox template.
#
# Cube boots the OCI image as the rootfs of a MicroVM, then probes the
# port given to `cubemastercli tpl create-from-image --probe`. Our
# entrypoint runs `dyson swarm` which reads SWARM_* env vars and
# brings up the HTTP controller on 0.0.0.0:80; swarm's host-based
# dyson_proxy then forwards `<id>.<sandbox_domain>` traffic to it.
#
# We use debian-slim instead of ghcr.io/tencentcloud/cubesandbox-base
# because the latter's anonymous pull is gated. envd (the cube file
# ops/exec helper) is therefore absent — fine for the smoke test where
# swarm only needs HTTP. Add envd back if/when sandbox file ops or
# `cube exec` matter.
#
# Build (uses prebuilt host binary at build/bin/dyson copied into context
# as `dyson-bin`):
#   docker build -t dyson:swarm -t 127.0.0.1:5000/dyson:swarm .
#
# Register with cube:
#   cubemastercli tpl create-from-image \
#       --image 127.0.0.1:5000/dyson:swarm \
#       --writable-layer-size 1G \
#       --expose-port 80 \
#       --probe 80 \
#       --probe-path /healthz

# ubuntu:24.04 (glibc 2.39) matches the build host. debian:bookworm-slim
# only ships glibc 2.36 and dies with `version GLIBC_2.39 not found` —
# would only be safe if dyson were built against the same older runtime
# (e.g. inside a bookworm builder container) or statically against musl.
FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*

COPY dyson-bin /usr/local/bin/dyson
RUN chmod 0755 /usr/local/bin/dyson

# Workspace lives at /var/lib/dyson; the swarm subcommand creates it on
# first boot. Pre-create with the right perms so the agent can write
# IDENTITY.md / TASK.md without elevating.
RUN mkdir -p /var/lib/dyson && chmod 0755 /var/lib/dyson

# jemalloc decay tuning recommended by the dyson README for memory-budgeted
# deployments — Cube cells are small so we want freed pages returned fast.
ENV MALLOC_CONF=dirty_decay_ms:1000,muzzy_decay_ms:1000

EXPOSE 80

# tini reaps zombies and forwards signals to dyson.
#
# The `dyson swarm` subcommand hardcodes dangerous-no-sandbox internally
# (Cube already provides the sandbox boundary; nesting another sandbox
# inside it is paranoia + a debug nightmare).  Pre-CLI-restructure the
# flag was a top-level `dyson --dangerous-no-sandbox swarm`; the newer
# CLI rejects unknown top-level flags, so the flag is gone from here.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/dyson"]
CMD ["swarm"]
