.PHONY: release_source_archive
release_source_archive:
	mkdir -p archives
	tar --exclude-vcs \
		--exclude=bazel-* \
		--exclude=target \
		--exclude=.github \
		--exclude=archives \
		-zcf "archives/release.tar.gz" .

.PHONY: release_deploy_jar
release_deploy_jar:
	bazel \
		build \
		//cli:bazel-diff_deploy.jar \
		-c opt

# Builds the same artifact CI publishes, named the same way:
# bazel-bin/release/bazel-diff-rust-<os>-<arch>[.exe].
.PHONY: release_rust_binary
release_rust_binary:
	bazel \
		build \
		//release:bazel-diff-rust \
		--config=release

.PHONY: build_rust
build_rust:
	bazel build //:bazel-diff-rust -c opt

.PHONY: format
format:
	bazel run //cli/format
	cargo fmt --all

.PHONY: generate-readme
generate-readme:
	bazel run //tools:generate-readme

.PHONY: coverage
coverage:
	bazel coverage --combined_report=lcov //cli/... //src:cli_tests //src:rust_tests //tools:coverage_check_test //tools/coverage/... //tools/go/...
	bazel run //tools:coverage-check -- bazel-out/_coverage/_coverage_report.dat
	bazel run //tools:coverage-check -- --include tools/go/ --threshold 90 bazel-out/_coverage/_coverage_report.dat

.PHONY: coverage-check
coverage-check:
	bazel run //tools:coverage-check -- bazel-out/_coverage/_coverage_report.dat
	bazel run //tools:coverage-check -- --include tools/go/ --threshold 90 bazel-out/_coverage/_coverage_report.dat

.PHONY: coverage-test
coverage-test:
	bazel test //tools:coverage_check_test

.PHONY: coverage-html
coverage-html:
	bazel coverage --combined_report=lcov //cli/... //src:cli_tests //src:rust_tests //tools:coverage_check_test //tools/coverage/... //tools/go/...
	bazel run //tools:coverage-check -- bazel-out/_coverage/_coverage_report.dat --html coverage-html
	@echo "Open coverage-html/index.html in a browser to inspect."

.PHONY: coverage_rust
coverage_rust:
	bazel coverage //src:cli_tests //src:rust_tests

.PHONY: benchmark
benchmark:
	@test -n "$(WORKSPACE)" || (echo "usage: make benchmark WORKSPACE=/path/to/bazel [BAZEL=/path/to/bazelisk] [HYPERFINE=/path/to/hyperfine] [STREAMED_PROTO=/path/to/targets.pb] [INCLUDE_BAZEL=1] [ITERATIONS=10] [WARMUP=3] [RSS_RUNS=5] [JSON=benchmark.json]" >&2; exit 2)
	$(or $(BAZEL),bazel) run -c opt //tools:benchmark -- \
		--workspace "$(WORKSPACE)" \
		--bazel "$(or $(BAZEL),bazel)" \
		--hyperfine "$(or $(HYPERFINE),hyperfine)" \
		--iterations "$(or $(ITERATIONS),10)" \
		--warmup "$(or $(WARMUP),3)" \
		--rss-runs "$(or $(RSS_RUNS),5)" \
		$(if $(STREAMED_PROTO),--streamed-proto "$(STREAMED_PROTO)",) \
		$(if $(INCLUDE_BAZEL),--include-bazel,) \
		$(if $(JSON),--json "$(JSON)",)

# Hermetic Kotlin-vs-Rust performance gate. Unlike `make benchmark` this needs no
# workspace, no Bazel server and no Hyperfine: it generates its own fixtures and fails
# (exit 1) if Rust is not faster than Kotlin on every workload. JSON=... must be an
# absolute path -- `bazel run` executes from the runfiles tree, not the repo root.
.PHONY: perf-gate
perf-gate:
	$(or $(BAZEL),bazel) run -c opt //tools:perf-gate -- \
		--rounds "$(or $(ROUNDS),5)" \
		--warmup-rounds "$(or $(WARMUP),1)" \
		--scale "$(or $(SCALE),1)" \
		$(if $(WORKLOAD),--workload "$(WORKLOAD)",) \
		$(if $(RSS_RUNS),--rss-runs "$(RSS_RUNS)",) \
		$(if $(JSON),--json "$(JSON)",)

.PHONY: perf-gate-test
perf-gate-test:
	$(or $(BAZEL),bazel) test //tools:perf_gate_test
