package com.bazel_diff.cli.converter

import assertk.assertThat
import assertk.assertions.isEqualTo
import org.junit.Assert.assertThrows
import org.junit.Test
import picocli.CommandLine.TypeConversionException

class ByteSizeConverterTest {
  private val converter = ByteSizeConverter()

  @Test
  fun parsesUnitsBase1024() {
    assertThat(converter.convert("1024")).isEqualTo(1024L)
    assertThat(converter.convert("100b")).isEqualTo(100L)
    assertThat(converter.convert("1k")).isEqualTo(1024L)
    assertThat(converter.convert("1kb")).isEqualTo(1024L)
    assertThat(converter.convert("500mb")).isEqualTo(500L * 1024 * 1024)
    assertThat(converter.convert("2g")).isEqualTo(2L * 1024 * 1024 * 1024)
    assertThat(converter.convert("10GB")).isEqualTo(10L * 1024 * 1024 * 1024)
    assertThat(converter.convert("1tb")).isEqualTo(1024L * 1024 * 1024 * 1024)
  }

  @Test
  fun toleratesWhitespaceAndCase() {
    assertThat(converter.convert(" 8 MB ")).isEqualTo(8L * 1024 * 1024)
  }

  @Test
  fun rejectsGarbage() {
    assertThrows(TypeConversionException::class.java) { converter.convert("") }
    assertThrows(TypeConversionException::class.java) { converter.convert("big") }
    assertThrows(TypeConversionException::class.java) { converter.convert("10zz") }
    assertThrows(TypeConversionException::class.java) { converter.convert("-5mb") }
  }
}
