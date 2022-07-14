package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.isEqualTo
import assertk.assertions.isNotEqualTo
import com.google.devtools.build.lib.query2.proto.proto2api.Build.Rule
import com.google.devtools.build.lib.query2.proto.proto2api.Build.Attribute
import org.junit.Test

class BazelRuleTest {
    @Test
    fun testHashDiffers() {
        val rule1Pb = Rule.newBuilder()
                .setRuleClass("java_library")
                .setName("libfoo")
                .build()

        val rule2Pb = Rule.newBuilder()
                .setRuleClass("java_library")
                .setName("libbar")
                .build()
        assertThat(BazelRule(rule1Pb).digest).isNotEqualTo(BazelRule(rule2Pb).digest)
    }
    @Test
    fun testIgnoreAttributes() {
        val rule1Pb = Rule.newBuilder()
                .setRuleClass("java_library")
                .setName("foo_library")
                .addAttribute(0, Attribute.newBuilder()
                        .setType(Attribute.Discriminator.STRING)
                        .setName("generator_location")
                        .setStringValue("path/to/BUILD:107:12").build())
                .build()

        val rule2Pb = Rule.newBuilder()
                .setRuleClass("java_library")
                .setName("foo_library")
                .addAttribute(0, Attribute.newBuilder()
                        .setType(Attribute.Discriminator.STRING)
                        .setName("generator_location")
                        .setStringValue("path/to/BUILD:111:1").build())
                .build()

        assertThat(BazelRule(rule1Pb).digest).isEqualTo(BazelRule(rule2Pb).digest)
    }
}