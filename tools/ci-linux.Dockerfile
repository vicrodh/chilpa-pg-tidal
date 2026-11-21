FROM rust:1.96-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends python3 build-essential pkg-config cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /source
ENV CARGO_BUILD_JOBS=2
