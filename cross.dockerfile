ARG VERSION="main"

FROM ghcr.io/cross-rs/aarch64-unknown-linux-gnu:${VERSION}

SHELL [ "/bin/sh", "-eu", "-c" ]

RUN \
  export DEBIAN_FRONTEND=noninteractive \
  ; dpkg --add-architecture arm64 \
  ; apt-get update \
  ; apt-get install --yes --no-install-recommends python3:arm64 python3-dev:arm64 libasound2:arm64 libasound2-dev:arm64 \
  ; apt-get clean \
  ; rm -rf /var/lib/apt/lists/*

ENV PYO3_CROSS="true"
ENV PYO3_CROSS_PYTHON_VERSION="3.8"
ENV PYO3_CROSS_PYTHON_IMPLEMENTATION="CPython"
