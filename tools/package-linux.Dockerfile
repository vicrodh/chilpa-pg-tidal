# The published Linux package is built on Debian 12, so it runs where glibc 2.36
# or newer is installed (Debian 12, Ubuntu 22.04 and later): a build on a newer
# system links newer glibc symbols.
FROM rust:1.96-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends python3 && rm -rf /var/lib/apt/lists/*
WORKDIR /source
