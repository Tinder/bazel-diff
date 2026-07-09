package com.bazel_diff.cli.converter

import assertk.assertThat
import assertk.assertions.isEqualTo
import java.time.Duration
import org.junit.Assert.assertThrows
import org.junit.Test
import picocli.CommandLine.TypeConversionException

class DurationConverterTest {
  private val converter = DurationConverter()

  @Test
  fun parsesSingleUnits() {
    assertThat(converter.convert("45s")).isEqualTo(Duration.ofSeconds(45))
    assertThat(converter.convert("90m")).isEqualTo(Duration.ofMinutes(90))
    assertThat(converter.convert("36h")).isEqualTo(Duration.ofHours(36))
    assertThat(converter.convert("7d")).isEqualTo(Duration.ofDays(7))
  }

  @Test
  fun parsesCombinationsAndTrimsAndIsCaseInsensitive() {
    assertThat(converter.convert("1d12h30m"))
        .isEqualTo(Duration.ofDays(1).plusHours(12).plusMinutes(30))
    assertThat(converter.convert(" 2H ")).isEqualTo(Duration.ofHours(2))
  }

  @Test
  fun rejectsBareNumbersAndGarbage() {
    // A bare number has no unit, so it is ambiguous and rejected rather than silently assumed.
    assertThrows(TypeConversionException::class.java) { converter.convert("7") }
    assertThrows(TypeConversionException::class.java) { converter.convert("") }
    assertThrows(TypeConversionException::class.java) { converter.convert("7x") }
    assertThrows(TypeConversionException::class.java) { converter.convert("1d 2h") }
    // Trailing digits with no unit must not be silently dropped.
    assertThrows(TypeConversionException::class.java) { converter.convert("1d2") }
  }
}
