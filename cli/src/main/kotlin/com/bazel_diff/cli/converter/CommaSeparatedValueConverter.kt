package com.bazel_diff.cli.converter

import picocli.CommandLine.ITypeConverter

class CommaSeparatedValueConverter : ITypeConverter<Set<String>> {
    override fun convert(value: String): Set<String> {
        return value.split(",").dropLastWhile { it.isEmpty() }.toSet()
    }
}
