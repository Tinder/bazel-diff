#!/bin/bash

# Get the current Git tag or default to the commit hash
echo "STABLE_GIT_TAG $(git describe --tags --abbrev=0 2>/dev/null || git rev-parse HEAD)"
