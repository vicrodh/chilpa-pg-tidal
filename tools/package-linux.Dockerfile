FROM python:3.13-slim-bookworm@sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641

RUN apt-get update && apt-get install -y --no-install-recommends binutils libflac12 \
    && rm -rf /var/lib/apt/lists/*
COPY requirements-lock.txt /tmp/requirements-lock.txt
RUN pip install --no-cache-dir -r /tmp/requirements-lock.txt
ENV CHILPA_FLAC_LIBRARY=/usr/lib/x86_64-linux-gnu/libFLAC.so.12
WORKDIR /source
