# Image for recording tmux compat goldens (task TC.0b, spec §9).
#
# Goldens are recorded only inside this container, never against the host's
# tmux. No distro ships tmux 3.7b exactly, so it is built from the release
# tarball, checked against a pinned sha256, and the build fails unless
# `tmux -V` prints `tmux 3.7b`.
#
# Built and run by scripts/record-compat-goldens.sh; see that script.

# rust:1.98-slim-trixie, pinned to its multi-arch index digest.
FROM docker.io/library/rust@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7

ARG TMUX_VERSION=3.7b
ARG TMUX_SHA256=87f2e99e3b685973f2ca002ffd6ed7e51a5744f7009daae5a15670b6d532db96

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bison ca-certificates curl libevent-dev libncurses-dev make pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSLo /tmp/tmux.tar.gz \
        "https://github.com/tmux/tmux/releases/download/${TMUX_VERSION}/tmux-${TMUX_VERSION}.tar.gz" \
    && echo "${TMUX_SHA256}  /tmp/tmux.tar.gz" | sha256sum -c - \
    && mkdir /tmp/tmux && tar -xzf /tmp/tmux.tar.gz -C /tmp/tmux --strip-components=1 \
    && cd /tmp/tmux && ./configure --prefix=/usr/local && make -j"$(nproc)" && make install \
    && rm -rf /tmp/tmux /tmp/tmux.tar.gz \
    && test "$(/usr/local/bin/tmux -V)" = "tmux ${TMUX_VERSION}"

# Record as an ordinary user: as root, the `word user` normalization would
# also rewrite tmux's `root` key table. The cargo cache and target dir are
# the user's, so the named volumes mounted there start out writable.
RUN useradd --create-home --uid 1000 golden \
    && mkdir -p /home/golden/cargo /target \
    && chown golden:golden /home/golden/cargo /target
ENV CARGO_HOME=/home/golden/cargo CARGO_TARGET_DIR=/target
USER golden
WORKDIR /src
