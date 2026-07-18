# JMT payment-EL image: overlay the JMT-patched release binary onto the stock arc_execution image.
# Build (after `cargo build --release --bin arc-node-execution` on branch gravity-payment-lane):
#   docker build -t arc_execution_jmt:latest -f experiments/dual-el/jmt/Dockerfile \
#       --build-arg BIN=target/release/arc-node-execution .
FROM arc_execution:latest
ARG BIN=target/release/arc-node-execution
COPY ${BIN} /usr/local/bin/arc-node-execution
