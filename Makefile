.PHONY: release_source_archive
release_source_archive:
	mkdir -p archives
	tar --exclude-vcs \
		--exclude=bazel-* \
		--exclude=.github \
		--exclude=archives \
		-zcf "archives/release.tar.gz" .
