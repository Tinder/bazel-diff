package com.bazel_diff.cli.converter

import com.bazel_diff.interactor.ExplainFormat
import picocli.CommandLine.ITypeConverter
import picocli.CommandLine.TypeConversionException

/**
 * Parses `explain --format` case-insensitively, so `--format dot` works as well as `--format DOT`.
 *
 * picocli's built-in enum conversion is case-sensitive, which would reject the lower-case spelling
 * everyone actually types (and the one used throughout the docs) for a Kotlin enum whose constants
 * are conventionally upper-case.
 */
class ExplainFormatConverter : ITypeConverter<ExplainFormat> {
  override fun convert(value: String): ExplainFormat =
      ExplainFormat.entries.firstOrNull { it.name.equals(value.trim(), ignoreCase = true) }
          ?: throw TypeConversionException(
              "invalid format '$value' (expected one of ${
                ExplainFormat.entries.joinToString(", ") { it.name.lowercase() }
              })")
}
