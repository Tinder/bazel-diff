# Use the same archive recipe as CI. Requires a git checkout because release
# archives omit .github/.
.PHONY: release_source_archive
release_source_archive:
	.github/workflows/pack_release_archive.sh archives/release.tar.gz

# Output: bazel-bin/release/bazel-diff-rust-<os>-<arch>[.exe].
.PHONY: release_rust_binary
release_rust_binary:
	bazel build //release:bazel-diff-rust --config=release

# Cross-compile static musl releases from Linux or Apple Silicon macOS.
.PHONY: release_rust_binary_linux
release_rust_binary_linux:
	bazel build //release:bazel-diff-rust --config=release-linux

.PHONY: release_rust_binary_linux_arm64
release_rust_binary_linux_arm64:
	bazel build //release:bazel-diff-rust --config=release-linux-arm64

.PHONY: build
build:
	bazel build //:bazel-diff -c opt

# Use the pinned rustfmt for the root crate and tools/coverage.
.PHONY: format
format:
	bazel run //tools/format:rustfmt

# Regenerate targets after adding, renaming, or removing tests under tests/e2e/.
# Commit the result; CI checks generated targets. See tools/e2e/README.md.
.PHONY: regen-e2e
regen-e2e:
	bazel run //tools/e2e:regen

# Check generated e2e targets without rewriting them.
.PHONY: regen-e2e-check
regen-e2e-check:
	bazel test //tools/e2e:regen_check //tools/e2e:split_e2e_tests_test

.PHONY: generate-readme
generate-readme:
	bazel run //tools:generate-readme

COVERAGE_TARGETS = //src:cli_tests //src:rust_tests //tools:coverage_check_test //tools/coverage/... //tools/go/...

.PHONY: coverage
coverage:
	bazel coverage --combined_report=lcov $(COVERAGE_TARGETS)
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
	bazel coverage --combined_report=lcov $(COVERAGE_TARGETS)
	bazel run //tools:coverage-check -- bazel-out/_coverage/_coverage_report.dat --html coverage-html
	@echo "Open coverage-html/index.html in a browser to inspect."
