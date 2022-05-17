package com.bazel_diff.cli.converter

import picocli.CommandLine.ITypeConverter
import java.io.File
import java.nio.file.Path

class NormalisingPathConverter : ITypeConverter<Path> {
    override fun convert(value: String): Path = File(value).toPath().normalize()
}
