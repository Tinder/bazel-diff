package com.bazel_diff.cli.converter

import assertk.assertThat
import assertk.assertions.isEqualTo
import org.junit.Test

class OptionsConverterTest {
    @Test
    fun testConverter() {
        val converter = OptionsConverter()
        assertThat(converter.convert("a b c d")).isEqualTo(listOf("a", "b", "c", "d"))
    }
}
