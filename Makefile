.PHONY: release_source_archive
release_source_archive:
	mkdir -p archives
	tar --exclude-vcs \
		--exclude=bazel-* \
		--exclude=.github \
		--exclude=archives \
		-zcf "archives/bazel_diff_$$(git describe --tags --abbrev=0 2>/dev/null || git rev-parse HEAD).tar.gz" .
