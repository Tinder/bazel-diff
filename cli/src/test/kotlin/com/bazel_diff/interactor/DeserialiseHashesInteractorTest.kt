package com.bazel_diff.interactor

import assertk.assertThat
import assertk.assertions.hasClass
import assertk.assertions.isEqualTo
import assertk.assertions.isFailure
import assertk.assertions.messageContains
import com.bazel_diff.testModule
import com.bazel_diff.hash.TargetHash
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule
import org.mockito.junit.MockitoJUnit

class DeserialiseHashesInteractorTest : KoinTest {
    @get:Rule
    val mockitoRule = MockitoJUnit.rule()

    @get:Rule
    val koinTestRule = KoinTestRule.create {
        modules(testModule())
    }

    @get:Rule
    val temp: TemporaryFolder = TemporaryFolder()

    val interactor = DeserialiseHashesInteractor()

    @Test
    fun testDeserialisation() {
        val file = temp.newFile().apply {
            writeText("""{"target-name":"hash#direct"}""")
        }

        val actual = interactor.executeTargetHash(file)
        assertThat(actual).isEqualTo(mapOf(
            "target-name" to TargetHash("", "hash", "direct")
        ))
    }

    @Test
    fun testDeserialisatingFileWithoutType() {
        val file = temp.newFile().apply {
            writeText("""{"target-name":"hash#direct"}""")
        }

        assertThat { interactor.executeTargetHash(file, setOf("Whatever"))}
            .isFailure().apply {
                messageContains("please re-generate the JSON with --includeTypeTarget!")
                hasClass(IllegalStateException::class)
            }
    }

    @Test
    fun testDeserialisationWithType() {
        val file = temp.newFile().apply {
            writeText("""{
                |  "target-1":"GeneratedFile#hash1#direct1", 
                |  "target-2":"Rule#hash2#direct2",
                |  "target-3":"SourceFile#hash3#direct3"
                |}""".trimMargin())
        }

        assertThat(interactor.executeTargetHash(file, null)).isEqualTo(mapOf(
            "target-1" to TargetHash("GeneratedFile", "hash1", "direct1"),
            "target-2" to TargetHash("Rule", "hash2", "direct2"),
            "target-3" to TargetHash("SourceFile", "hash3", "direct3")
        ))
        assertThat(interactor.executeTargetHash(file, setOf("GeneratedFile"))).isEqualTo(mapOf(
            "target-1" to TargetHash("GeneratedFile", "hash1", "direct1")
        ))
        assertThat(interactor.executeTargetHash(file, setOf("Rule"))).isEqualTo(mapOf(
            "target-2" to TargetHash("Rule", "hash2", "direct2")
        ))
        assertThat(interactor.executeTargetHash(file, setOf("SourceFile"))).isEqualTo(mapOf(
            "target-3" to TargetHash("SourceFile", "hash3", "direct3")
        ))
    }
}
