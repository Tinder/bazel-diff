# Linux benchmark image for the Firecracker snapshot harness.
#
# Runs the benchmark and local driver in Linux. Firecracker requires /dev/kvm.
# The JDK is required by Bazel.
FROM eclipse-temurin:21-jdk

ARG BAZELISK_ARCH=arm64
ARG BAZELISK_VERSION=v1.20.0

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git python3 bash curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSL -o /usr/local/bin/bazelisk \
        "https://github.com/bazelbuild/bazelisk/releases/download/${BAZELISK_VERSION}/bazelisk-linux-${BAZELISK_ARCH}" \
    && chmod +x /usr/local/bin/bazelisk \
    && ln -sf /usr/local/bin/bazelisk /usr/local/bin/bazel

# Static musl release for the container's architecture.
COPY bazel-diff /usr/local/bin/bazel-diff
RUN chmod +x /usr/local/bin/bazel-diff

# Go orchestrator and benchmark scripts.
COPY bazel-diff-snap /usr/local/bin/bazel-diff-snap
COPY gen_project.py bench.py run_in_container.sh /opt/bench/
RUN chmod +x /usr/local/bin/bazel-diff-snap /opt/bench/run_in_container.sh

WORKDIR /work
ENTRYPOINT ["/opt/bench/run_in_container.sh"]
