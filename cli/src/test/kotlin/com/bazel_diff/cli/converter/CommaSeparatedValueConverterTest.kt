package com.bazel_diff.cli.converter

import assertk.assertThat
import assertk.assertions.isEqualTo
import org.junit.Test

class CommaSeparatedValueConverterTest {
    @Test
    fun testConverter() {
        val converter = CommaSeparatedValueConverter()
        assertThat(converter.convert("a,b,c,d")).isEqualTo(listOf("a", "b", "c", "d"))
    }
}
