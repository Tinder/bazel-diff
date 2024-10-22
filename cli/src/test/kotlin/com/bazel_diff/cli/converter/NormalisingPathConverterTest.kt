package com.bazel_diff.cli.converter

import assertk.assertThat
import assertk.assertions.isEqualTo
import java.nio.file.Paths
import org.junit.Test

class NormalisingPathConverterTest {

  @Test
  fun testConvert() {
    val converter = NormalisingPathConverter()
    assertThat(converter.convert("/home/../some-dir")).isEqualTo(Paths.get("/some-dir"))
  }
}
