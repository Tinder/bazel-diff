package com.bazel_diff.cli.converter

import java.io.File
import java.nio.file.Path
import picocli.CommandLine.ITypeConverter

class NormalisingPathConverter : ITypeConverter<Path> {
  override fun convert(value: String): Path = File(value).toPath().normalize()
}
