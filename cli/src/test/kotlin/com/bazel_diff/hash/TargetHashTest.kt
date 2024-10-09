package com.bazel_diff.hash

import assertk.assertThat
import assertk.assertions.*
import org.junit.Test


class TargetHashTest {

    @Test
    fun testRoundTripJson() {
        val th = TargetHash(
            "Rule",
            "hash",
            "directHash",
        )
        assertThat(TargetHash.fromJson(th.toJson(includeTargetType = true))).isEqualTo(th)
    }

    @Test
    fun testRoundTripJsonWithoutType() {
        val th = TargetHash(
            "Rule",
            "hash",
            "directHash",
        )
        assertThat(th.hasType()).isTrue()

        val newTh = TargetHash.fromJson(th.toJson(includeTargetType = false))

        assertThat(newTh.type).isEqualTo("")
        assertThat(newTh.hash).isEqualTo(th.hash)
        assertThat(newTh.directHash).isEqualTo(th.directHash)

        assertThat(newTh.hasType()).isFalse()
    }

    @Test
    fun testInvalidFromJson() {

        
        assertThat {
            TargetHash.fromJson("invalid")
        }.isFailure()

        assertThat {
            TargetHash.fromJson("")
        }.isFailure()

        assertThat {
            TargetHash.fromJson("too#many#delimeters#here#")
        }.isFailure()


    }
}

