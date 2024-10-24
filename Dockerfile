# syntax = docker/dockerfile:1.7

FROM rust:1.81.0-bookworm as builder
ARG TARGETARCH
ARG TARGETPLATFORM

WORKDIR /app

RUN mkdir -p -m 0600 ~/.ssh && ssh-keyscan git.corp.adobe.com >> ~/.ssh/known_hosts

RUN <<eof
#!/bin/bash
  apt-get update
  DEBIAN_FRONTEND=noninteractive apt-get install --assume-yes wget unzip curl build-essential pkg-config openssl ca-certificates openssh-client libssl-dev protobuf-compiler patchelf python3
  DEBIAN_FRONTEND=noninteractive apt-get install --assume-yes python3.11-venv

  # mkdir -p /var/tmp/proto
  # pushd /var/tmp/proto
  # if [[ "${TARGETARCH}" == "amd64" ]]; then
  #   protoc_file=protoc-26.1-linux-x86_64.zip
  # else
  #   protoc_file=protoc-26.1-linux-aarch_64.zip
  # fi
  # wget https://github.com/protocolbuffers/protobuf/releases/download/v26.1/${protoc_file}
  # unzip ${protoc_file}
  # cp -a ./bin/* /usr/bin/
  # cp -a ./include/* /usr/include/
eof

COPY . .
# on the rust image, the CARGO_HOME is set to /usr/local/cargo
RUN --mount=type=cache,target=/usr/local/cargo,from=rust,source=/usr/local/cargo \
    --mount=type=cache,target=/app/target \
    --mount=type=ssh \
<<eof
#!/bin/bash
python3 -m venv venv
source venv/bin/activate
pip3 install -r requirements.in
maturin build --release
cp /app/target/wheels/*.whl /app/
eof

#RUN --mount=type=cache,target=/root/.yarn \
#<<eof
##!/bin/bash
#  export YARN_CACHE_FOLDER=/root/.yarn
#  pushd /app/ballista/scheduler/ui
#  yarnpkg install
#  yarnpkg build
#eof

FROM debian:bookworm-slim AS runtime

COPY --from=builder /app/*.whl /

WORKDIR /app


