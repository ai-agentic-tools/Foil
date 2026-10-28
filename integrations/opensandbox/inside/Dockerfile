# An OpenSandbox sandbox image with foil in it: branch the project inside
# the sandbox, try several attempts at once, review, and commit the best.
#
#   docker build -t foil-opensandbox integrations/opensandbox/inside
#
# Create sandboxes from it with the extension `bootstrap.execd.isolation`
# set to `enable` (see README.md): that gives the container what foil needs.
FROM ubuntu:24.04

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git python3 \
 && rm -rf /var/lib/apt/lists/*

# A release binary (static, x86_64), checked against its published SHA-256.
ARG FOIL_VERSION=latest
RUN set -eu; \
    case $FOIL_VERSION in latest) base=https://github.com/ai-agentic-tools/foil/releases/latest/download ;; \
         *) base=https://github.com/ai-agentic-tools/foil/releases/download/$FOIL_VERSION ;; esac; \
    cd /tmp && curl -fsSLO $base/foil-x86_64-linux && curl -fsSLO $base/foil-x86_64-linux.sha256 \
    && sha256sum -c foil-x86_64-linux.sha256 \
    && install -m 0755 foil-x86_64-linux /usr/local/bin/foil && rm foil-x86_64-linux*

# Branches live on the tmpfs OpenSandbox mounts for isolated execution: the
# container's own filesystem is overlayfs, which a branch can't stack on.
ENV FOIL_HOME=/var/lib/execd/isolation/foil
