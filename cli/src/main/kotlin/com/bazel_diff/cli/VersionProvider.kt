package com.bazel_diff.cli

import picocli.CommandLine.IVersionProvider

class VersionProvider : IVersionProvider {
    override fun getVersion(): Array<String> {
        return arrayOf("7.0.0")
    }
}
