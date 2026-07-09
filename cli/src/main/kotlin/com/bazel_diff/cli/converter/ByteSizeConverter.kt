package com.bazel_diff.cli.converter

import picocli.CommandLine.ITypeConverter
import picocli.CommandLine.TypeConversionException

/**
 * Parses a human-friendly byte size such as `10GB`, `500MB`, `2g`, `1kb`, or a bare `1048576`
 * (bytes when no unit is given). Units are base 1024 and case-insensitive: `b`, `k`/`kb`, `m`/`mb`,
 * `g`/`gb`, `t`/`tb`. Returns the size in bytes.
 */
class ByteSizeConverter : ITypeConverter<Long> {
  override fun convert(value: String): Long {
    val match =
        PATTERN.matchEntire(value.trim().lowercase())
            ?: throw TypeConversionException(
                "invalid size '$value' (expected e.g. 1048576, 500mb, 10gb)")
    val amount = match.groupValues[1].toLong()
    val multiplier =
        when (match.groupValues[2]) {
          "",
          "b" -> 1L
          "k",
          "kb" -> KB
          "m",
          "mb" -> KB * KB
          "g",
          "gb" -> KB * KB * KB
          "t",
          "tb" -> KB * KB * KB * KB
          else -> throw TypeConversionException("invalid size unit in '$value'")
        }
    return amount * multiplier
  }

  private companion object {
    const val KB = 1024L
    val PATTERN = Regex("(\\d+)\\s*([a-z]*)")
  }
}
