# Delegates rather than repeating the tar invocation: release_prep.sh (release),
# bcr_consumer.yaml and ci.yaml all pack through this one script, so the archive
# you get locally is byte-for-byte the recipe that ships. The inlined copy that
# used to live here had already drifted from it.
# Note: the script lives under .github/, which the archive itself excludes, so
# this target only works in a git checkout -- not inside an extracted release.
.PHONY: release_source_archive
release_source_archive:
	.github/workflows/pack_release_archive.sh archives/release.tar.gz

# Builds the same artifact CI publishes, named the same way:
# bazel-bin/release/bazel-diff-rust-<os>-<arch>[.exe].
.PHONY: release_rust_binary
release_rust_binary:
	bazel \
		build \
		//release:bazel-diff-rust \
		--config=release

# The published Linux binaries, which are not host-native: they are statically
# linked against musl so they run on any distribution, and cross-compile from a
# glibc Linux host or an Apple Silicon Mac. Same output path and asset name.
.PHONY: release_rust_binary_linux
release_rust_binary_linux:
	bazel \
		build \
		//release:bazel-diff-rust \
		--config=release-musl

.PHONY: release_rust_binary_linux_arm64
release_rust_binary_linux_arm64:
	bazel \
		build \
		//release:bazel-diff-rust \
		--config=release-musl-arm64

.PHONY: build
build:
	bazel build //:bazel-diff -c opt

# Goes through Bazel so it uses the same rustfmt CI gates on. `cargo fmt --all`
# is not equivalent: it only sees the root crate, missing tools/coverage, and
# it uses whatever rustfmt is on PATH rather than the pinned one.
.PHONY: format
format:
	bazel run //tools/format:rustfmt

# Regenerates the per-case e2e test targets from the e2e sources. Run it after
# adding, renaming or removing a `#[test]` fn under tests/e2e/, and commit the
# result -- `e2e-split-regen` in ci.yaml fails the build if the checked-in
# split is stale. See tools/e2e/README.md.
.PHONY: regen-e2e
regen-e2e:
	bazel run //tools/e2e:regen

# What CI runs. Reports staleness; it never rewrites anything.
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
