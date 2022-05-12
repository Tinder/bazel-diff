package com.bazel_diff.cli.converter

import picocli.CommandLine.ITypeConverter

class OptionsConverter : ITypeConverter<List<String>> {
    override fun convert(value: String): List<String> {
        return value.split(" ").dropLastWhile { it.isEmpty() }
    }
}
