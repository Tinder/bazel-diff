package com.bazel_diff.cli.converter

import java.time.Duration
import picocli.CommandLine.ITypeConverter
import picocli.CommandLine.TypeConversionException

/**
 * Parses a human-friendly duration such as `7d`, `36h`, `90m`, `45s`, or a combination like
 * `1d12h30m`. Recognised unit suffixes are `d` (days), `h` (hours), `m` (minutes) and `s`
 * (seconds). The whole string must be made up of `<number><unit>` tokens; anything else is rejected
 * so a typo fails fast rather than being silently truncated.
 */
class DurationConverter : ITypeConverter<Duration> {
  override fun convert(value: String): Duration {
    val normalized = value.trim().lowercase()
    val tokens = TOKEN.findAll(normalized).toList()
    val consumed = tokens.sumOf { it.value.length }
    if (tokens.isEmpty() || consumed != normalized.length) {
      throw TypeConversionException(
          "invalid duration '$value' (expected e.g. 7d, 36h, 90m, 45s, or 1d12h)")
    }
    var total = Duration.ZERO
    for (token in tokens) {
      val amount = token.groupValues[1].toLong()
      total +=
          when (token.groupValues[2]) {
            "d" -> Duration.ofDays(amount)
            "h" -> Duration.ofHours(amount)
            "m" -> Duration.ofMinutes(amount)
            "s" -> Duration.ofSeconds(amount)
            else -> throw TypeConversionException("invalid duration unit in '$value'")
          }
    }
    return total
  }

  private companion object {
    val TOKEN = Regex("(\\d+)([dhms])")
  }
}
