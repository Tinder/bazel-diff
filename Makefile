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
	bazelisk \
		build \
		//cli:bazel-diff_deploy.jar \
		-c opt

.PHONY: format
format:
	bazelisk run //cli/format