.PHONY: release_source_archive
release_source_archive:
	mkdir -p archives
	tar --exclude-vcs \
		--exclude=bazel-* \
		--exclude=.github \
		--exclude=archives \
		-zcf "archives/release.tar.gz" .

.PHONY: release_deploy_jar
release_deploy_jar:
	bazel \
		build \
		//cli:bazel-diff_deploy.jar \
		-c opt

.PHONY: format
format:
	bazel run //cli/format

.PHONY: generate-readme
generate-readme:
	bazel run //tools:generate-readme

.PHONY: coverage
coverage:
	bazel coverage --combined_report=lcov //cli/... //tools:coverage_check_test
	bazel run //tools:coverage-check -- bazel-out/_coverage/_coverage_report.dat

.PHONY: coverage-check
coverage-check:
	bazel run //tools:coverage-check -- bazel-out/_coverage/_coverage_report.dat

.PHONY: coverage-test
coverage-test:
	bazel test //tools:coverage_check_test

.PHONY: coverage-html
coverage-html:
	bazel coverage --combined_report=lcov //cli/... //tools:coverage_check_test
	bazel run //tools:coverage-check -- bazel-out/_coverage/_coverage_report.dat --html coverage-html
	@echo "Open coverage-html/index.html in a browser to inspect."
