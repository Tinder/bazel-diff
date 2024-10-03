.PHONY: release_source_archive
release_source_archive: version ?= $$(git describe --tags --abbrev=0 2>/dev/null || git rev-parse HEAD)
release_source_archive:
	mkdir -p archives
	tar --exclude-vcs \
		--exclude=bazel-* \
		--exclude=.github \
		--exclude=archives \
		-zcf "archives/bazel_diff_$(version).tar.gz" .
