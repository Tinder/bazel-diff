#!/bin/bash
set +e

# Get the current Git tag
echo "STABLE_GIT_TAG $(git describe --tags --abbrev=0 2>/dev/null || git rev-parse HEAD)"
