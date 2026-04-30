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
# Register with cube (resource flags come from deploy/config.env via
# bring-up.sh's `register_cube_template` helper; the values shown here
# are today's defaults):
#   cubemastercli tpl create-from-image \
#       --image 127.0.0.1:5000/dyson:swarm \
#       --writable-layer-size 5G \
#       --cpu 2000 \
#       --memory 2000 \
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
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        tini \
        git \
        curl \
        wget \
        openssh-client \
        build-essential \
        pkg-config \
        make \
        patch \
        python3 \
        python3-pip \
        python3-venv \
        nodejs \
        npm \
        rustc \
        cargo \
        jq \
        vim \
        less \
        tree \
        unzip \
        zip \
        xz-utils \
        gzip \
        tar \
        file \
        procps \
        htop \
        lsof \
        iputils-ping \
        iputils-tracepath \
        traceroute \
        mtr-tiny \
        dnsutils \
        iproute2 \
        net-tools \
        ncat \
        telnet \
        whois \
        openssl \
        tcpdump \
        iperf3 \
        sysstat \
        strace \
    && rm -rf /var/lib/apt/lists/*

# `--chmod=0755` folds the COPY + chmod into one layer.  Without it
# the chmod RUN copies the 48 MB binary again into a fresh layer, so
# the image carries two 48 MB copies of the same bytes — Docker
# dedupes within a local image but not across registry pulls, so
# every `docker push` ate the duplicate over the wire.
COPY --chmod=0755 dyson-bin /usr/local/bin/dyson

# Workspace lives at /var/lib/dyson; the swarm subcommand creates it on
# first boot. Pre-create with the right perms so the agent can write
# IDENTITY.md / TASK.md without elevating.
RUN mkdir -p /var/lib/dyson && chmod 0755 /var/lib/dyson

# jemalloc decay tuning recommended by the dyson README for memory-budgeted
# deployments — Cube cells are small so we want freed pages returned fast.
ENV MALLOC_CONF=dirty_decay_ms:1000,muzzy_decay_ms:1000

# HTTP/HTTPS forward proxy for outbound traffic.  The cube's eBPF SNAT
# uses bpf_redirect which bypasses the host kernel's TCP stack, and
# some upstream networks (Google, GitHub via Microsoft) silently drop
# return packets for those flows.  Routing TCP through the
# host-resident tinyproxy at mvm_gateway_ip:3128 makes the connection
# originate from the host's kernel stack so every destination accepts
# it.  These vars must be baked into the image because the cube
# template's snapshot freezes /proc/<dyson>/environ at warmup time —
# per-instance envVars passed by swarm at create-time never reach the
# running process.  Per-policy gating happens host-side: tinyproxy
# only accepts CONNECT from cube IPs whose policy allows public
# egress (the swarm rewrites /etc/tinyproxy/cube-allow.conf on every
# instance lifecycle event), so a cube under Airgap or Allowlist
# carries the env but the proxy returns 403 — no internet leak.
# NO_PROXY keeps swarm /llm and the local CoreDNS resolver direct.
ENV HTTPS_PROXY=http://169.254.68.5:3128
ENV HTTP_PROXY=http://169.254.68.5:3128
ENV https_proxy=http://169.254.68.5:3128
ENV http_proxy=http://169.254.68.5:3128
ENV NO_PROXY=169.254.68.5,169.254.254.53,127.0.0.1,localhost
ENV no_proxy=169.254.68.5,169.254.254.53,127.0.0.1,localhost

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
