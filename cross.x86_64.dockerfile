ARG VERSION="main"

FROM ghcr.io/cross-rs/x86_64-unknown-linux-gnu:${VERSION}

SHELL [ "/bin/sh", "-eu", "-c" ]

RUN \
  export DEBIAN_FRONTEND=noninteractive \
  ; apt-get update \
  ; apt-get install --yes --no-install-recommends python3 python3-dev libasound2 libasound2-dev \
  ; apt-get clean \
  ; rm -rf /var/lib/apt/lists/*

ENV PYO3_CROSS="true"
ENV PYO3_CROSS_PYTHON_VERSION="3.8"
ENV PYO3_CROSS_PYTHON_IMPLEMENTATION="CPython"

