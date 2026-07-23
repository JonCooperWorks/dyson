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
#       --writable-layer-size 8G \
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
ARG NUCLEI_VERSION=3.11.0
ARG NUCLEI_SHA256=dc238d6040813e14fc30514dac5a2eb1b430c694f3ca99eee2a5097e55076283
ARG HTTPX_VERSION=1.10.0
ARG HTTPX_SHA256=63eac4dcd6e5c9867c94765fdaaf66e7b4eeae3474a1f06e600e266a1c81a53e
ARG KATANA_VERSION=1.6.1
ARG KATANA_SHA256=503754f1bd370c3ef287df6998e317baed2dd75bdd13ea64034f09b80ca393f3
ARG FFUF_VERSION=2.2.1
ARG FFUF_SHA256=86307885810d3c36ba4a3e9ba5178c2d9027bba0dd7f4ea39e39e7c972b62396
ARG NUCLEI_TEMPLATES_VERSION=10.4.6
ARG NUCLEI_TEMPLATES_SHA256=ab24c96eccf4a9dc531c9054d54a820854c971269a8185deba57927015e208f9
ARG TESTSSL_VERSION=3.2.4
ARG TESTSSL_SHA256=98528f8a0ac07f1e226efaa8ead438247df8efcb8fee4e056a937ab82a305490
ARG PLAYWRIGHT_VERSION=1.61.0

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
        nmap \
        nikto \
        sqlmap \
        gobuster \
        dirb \
        wfuzz \
        whatweb \
        wapiti \
        dnsrecon \
        smbclient \
        ldap-utils \
        snmp \
        redis-tools \
        postgresql-client \
        mariadb-client \
    && rm -rf \
        /var/lib/apt/lists/* \
        /usr/share/doc \
        /usr/share/man \
        /usr/share/locale \
    && find /usr -type d -name __pycache__ -prune -exec rm -rf {} +

# Pinned, checksum-verified web discovery stack. These release binaries keep
# the runtime image free of a Go toolchain and make image rebuilds independent
# of upstream `latest` tags. The Cube fleet is currently amd64-only: dyson-bin
# is copied from the amd64 deployment host, so matching scanner assets is
# intentional rather than an incomplete multi-arch claim.
RUN set -eux; \
    work=/tmp/dyson-pentest-tools; \
    mkdir -p "${work}"; \
    curl -fsSL --retry 3 \
        "https://github.com/projectdiscovery/nuclei/releases/download/v${NUCLEI_VERSION}/nuclei_${NUCLEI_VERSION}_linux_amd64.zip" \
        -o "${work}/nuclei.zip"; \
    echo "${NUCLEI_SHA256}  ${work}/nuclei.zip" | sha256sum -c -; \
    unzip -q "${work}/nuclei.zip" -d "${work}/nuclei"; \
    install -m 0755 "${work}/nuclei/nuclei" /usr/local/bin/nuclei; \
    curl -fsSL --retry 3 \
        "https://github.com/projectdiscovery/httpx/releases/download/v${HTTPX_VERSION}/httpx_${HTTPX_VERSION}_linux_amd64.zip" \
        -o "${work}/httpx.zip"; \
    echo "${HTTPX_SHA256}  ${work}/httpx.zip" | sha256sum -c -; \
    unzip -q "${work}/httpx.zip" -d "${work}/httpx"; \
    install -m 0755 "${work}/httpx/httpx" /usr/local/bin/httpx; \
    curl -fsSL --retry 3 \
        "https://github.com/projectdiscovery/katana/releases/download/v${KATANA_VERSION}/katana_${KATANA_VERSION}_linux_amd64.zip" \
        -o "${work}/katana.zip"; \
    echo "${KATANA_SHA256}  ${work}/katana.zip" | sha256sum -c -; \
    unzip -q "${work}/katana.zip" -d "${work}/katana"; \
    install -m 0755 "${work}/katana/katana" /usr/local/bin/katana; \
    curl -fsSL --retry 3 \
        "https://github.com/ffuf/ffuf/releases/download/v${FFUF_VERSION}/ffuf_${FFUF_VERSION}_linux_amd64.tar.gz" \
        -o "${work}/ffuf.tar.gz"; \
    echo "${FFUF_SHA256}  ${work}/ffuf.tar.gz" | sha256sum -c -; \
    tar -xzf "${work}/ffuf.tar.gz" -C "${work}"; \
    install -m 0755 "${work}/ffuf" /usr/local/bin/ffuf; \
    find "${work}" -delete

# Keep the vulnerability knowledge shipped with the image immutable and
# reviewable. Nuclei discovers ~/nuclei-templates by default; the symlink makes
# the pinned tree the default while `-t /opt/nuclei-templates` stays explicit.
# dirb's compact packaged wordlists live under /usr/share/dirb/wordlists.
RUN set -eux; \
    work=/tmp/dyson-pentest-data; \
    mkdir -p "${work}" /opt/nuclei-templates /opt/testssl; \
    curl -fsSL --retry 3 \
        "https://github.com/projectdiscovery/nuclei-templates/archive/refs/tags/v${NUCLEI_TEMPLATES_VERSION}.tar.gz" \
        -o "${work}/nuclei-templates.tar.gz"; \
    echo "${NUCLEI_TEMPLATES_SHA256}  ${work}/nuclei-templates.tar.gz" | sha256sum -c -; \
    tar -xzf "${work}/nuclei-templates.tar.gz" \
        --strip-components=1 -C /opt/nuclei-templates; \
    curl -fsSL --retry 3 \
        "https://github.com/testssl/testssl.sh/archive/refs/tags/v${TESTSSL_VERSION}.tar.gz" \
        -o "${work}/testssl.tar.gz"; \
    echo "${TESTSSL_SHA256}  ${work}/testssl.tar.gz" | sha256sum -c -; \
    tar -xzf "${work}/testssl.tar.gz" --strip-components=1 -C /opt/testssl; \
    ln -s /opt/nuclei-templates /root/nuclei-templates; \
    ln -s /opt/testssl/testssl.sh /usr/local/bin/testssl; \
    find "${work}" -delete

# Browser automation is isolated in a venv so Ubuntu's system Python remains
# untouched. PENTEST_PYTHON is the stable entry point for scripts importing
# playwright; the browser payload is shared under /opt instead of root's cache.
ENV PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright
ENV PENTEST_PYTHON=/opt/pentest-venv/bin/python
RUN python3 -m venv /opt/pentest-venv \
    && /opt/pentest-venv/bin/pip install --no-cache-dir \
        "playwright==${PLAYWRIGHT_VERSION}" \
    && /opt/pentest-venv/bin/playwright install --with-deps chromium \
    && ln -s /opt/pentest-venv/bin/playwright /usr/local/bin/playwright \
    && rm -rf /var/lib/apt/lists/*

# Fail the image build if any promised capability disappears because an
# archive layout or package name changed. Version output is intentionally
# suppressed; existence and executable linkage are the contract here.
RUN set -eux; \
    for tool in \
        curl ncat nmap nikto sqlmap gobuster dirb wfuzz whatweb wapiti \
        dnsrecon smbclient ldapsearch snmpwalk redis-cli psql mariadb \
        nuclei httpx katana ffuf testssl playwright; \
    do \
        command -v "${tool}" >/dev/null; \
    done; \
    test -d /opt/nuclei-templates/http; \
    test -d /usr/share/dirb/wordlists; \
    "${PENTEST_PYTHON}" -c \
        'from pathlib import Path; from playwright.sync_api import sync_playwright; p = sync_playwright().start(); assert Path(p.chromium.executable_path).is_file(); p.stop()'

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
# host-resident dyson-egress-proxy at mvm_gateway_ip:3128 makes the
# connection originate from the host's kernel stack so every destination
# accepts it.  These vars must be baked into the image because the cube
# template's snapshot freezes /proc/<dyson>/environ at warmup time —
# per-instance envVars passed by swarm at create-time never reach the
# running process.  Per-policy gating happens host-side: the proxy
# loads /run/dyson-egress/policies.json and checks both the source
# sandbox IP and destination IPs before dialing, so proxy egress is
# never broader than the sandbox's declared network policy.
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
