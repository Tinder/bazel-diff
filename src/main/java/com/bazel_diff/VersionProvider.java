package com.bazel_diff;

import picocli.CommandLine.IVersionProvider;

class VersionProvider implements IVersionProvider {
    public String[] getVersion() throws Exception {
        return new String[] {
            "2.1.1"
        };
    }
}
