package com.bazel_diff.interactor

import assertk.assertThat
import assertk.assertions.hasClass
import assertk.assertions.isEqualTo
import assertk.assertions.isFailure
import assertk.assertions.messageContains
import com.bazel_diff.testModule
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
            writeText("""{"target-name":"hash"}""")
        }

        val actual = interactor.execute(file)
        assertThat(actual).isEqualTo(mapOf(
            "target-name" to "hash"
        ))
    }

    @Test
    fun testDeserialisatingFileWithoutType() {
        val file = temp.newFile().apply {
            writeText("""{"target-name":"hash"}""")
        }

        assertThat { interactor.execute(file, setOf("Whatever"))}
            .isFailure().apply {
                messageContains("please re-generate the JSON with --includeTypeTarget!")
                hasClass(IllegalStateException::class)
            }
    }

    @Test
    fun testDeserialisationWithType() {
        val file = temp.newFile().apply {
            writeText("""{
                |  "target-1":"GeneratedFile#hash1", 
                |  "target-2":"Rule#hash2",
                |  "target-3":"SourceFile#hash3"
                |}""".trimMargin())
        }

        assertThat(interactor.execute(file, null)).isEqualTo(mapOf(
            "target-1" to "hash1",
            "target-2" to "hash2",
            "target-3" to "hash3"
        ))
        assertThat(interactor.execute(file, setOf("GeneratedFile"))).isEqualTo(mapOf(
            "target-1" to "hash1"
        ))
        assertThat(interactor.execute(file, setOf("Rule"))).isEqualTo(mapOf(
            "target-2" to "hash2"
        ))
        assertThat(interactor.execute(file, setOf("SourceFile"))).isEqualTo(mapOf(
            "target-3" to "hash3"
        ))
    }
}
