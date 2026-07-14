# Minimal wrapper for a host-built gravity-reth binary (github.com/Galxe/gravity-reth).
# Build:  docker build -t gravity_reth:latest -f experiments/dual-el/gravity/Dockerfile \
#           --build-arg BIN=target/release/reth /home/papaduck/gravity-reth
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
ARG BIN=target/release/reth
COPY ${BIN} /usr/local/bin/gravity-reth
ENTRYPOINT ["/usr/local/bin/gravity-reth"]
