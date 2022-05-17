package com.bazel_diff.cli.converter

import assertk.assertThat
import assertk.assertions.isEqualTo
import org.junit.Test
import java.nio.file.Paths

class NormalisingPathConverterTest {

    @Test
    fun testConvert() {
        val converter = NormalisingPathConverter()
        assertThat(converter.convert("/home/../some-dir")).isEqualTo(Paths.get("/some-dir"))
    }
}
