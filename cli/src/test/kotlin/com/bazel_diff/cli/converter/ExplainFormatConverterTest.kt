package com.bazel_diff.cli.converter

import assertk.assertFailure
import assertk.assertThat
import assertk.assertions.hasMessage
import assertk.assertions.isEqualTo
import assertk.assertions.isInstanceOf
import com.bazel_diff.interactor.ExplainFormat
import org.junit.Test
import picocli.CommandLine.TypeConversionException

class ExplainFormatConverterTest {
  private val converter = ExplainFormatConverter()

  @Test
  fun acceptsTheLowerCaseSpellingUsedInTheDocs() {
    assertThat(converter.convert("dot")).isEqualTo(ExplainFormat.DOT)
    assertThat(converter.convert("mermaid")).isEqualTo(ExplainFormat.MERMAID)
    assertThat(converter.convert("json")).isEqualTo(ExplainFormat.JSON)
    assertThat(converter.convert("text")).isEqualTo(ExplainFormat.TEXT)
  }

  @Test
  fun acceptsTheEnumSpellingAndMixedCaseAndSurroundingSpace() {
    assertThat(converter.convert("DOT")).isEqualTo(ExplainFormat.DOT)
    assertThat(converter.convert("MerMaid")).isEqualTo(ExplainFormat.MERMAID)
    assertThat(converter.convert("  json  ")).isEqualTo(ExplainFormat.JSON)
  }

  @Test
  fun rejectsAnUnknownFormatAndListsTheValidOnes() {
    assertFailure { converter.convert("svg") }
        .isInstanceOf(TypeConversionException::class)
        .hasMessage("invalid format 'svg' (expected one of text, json, dot, mermaid)")
  }
}
