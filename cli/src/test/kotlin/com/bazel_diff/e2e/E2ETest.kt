package com.bazel_diff.e2e

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.containsExactlyInAnyOrder
import assertk.assertions.containsOnly
import assertk.assertions.isEqualTo
import com.bazel_diff.Main
import com.bazel_diff.cli.BazelDiff
import com.bazel_diff.interactor.DeserialiseHashesInteractor
import com.bazel_diff.testModule
import com.google.gson.Gson
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.koin.core.context.startKoin
import picocli.CommandLine
import java.io.File
import java.io.FileInputStream
import java.io.IOException
import java.nio.file.Path
import java.nio.file.Paths
import java.nio.file.StandardCopyOption
import java.util.zip.ZipFile
import java.util.zip.ZipInputStream


class E2ETest {
    @get:Rule
    val temp: TemporaryFolder = TemporaryFolder()

    @Test
    fun testE2E() {
        val projectA = extractFixtureProject("/fixture/integration-test-1.zip")
        val projectB = extractFixtureProject("/fixture/integration-test-2.zip")

        val workingDirectoryA = File(projectA, "integration")
        val workingDirectoryB = File(projectB, "integration")
        val bazelPath = "bazel"
        val outputDir = temp.newFolder()
        val from = File(outputDir, "starting_hashes.json")
        val to = File(outputDir, "final_hashes.json")
        val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

        val cli = CommandLine(BazelDiff())
        //From
        cli.execute(
            "generate-hashes", "-w", workingDirectoryA.absolutePath, "-b", bazelPath, from.absolutePath
        )
        //To
        cli.execute(
            "generate-hashes", "-w", workingDirectoryB.absolutePath, "-b", bazelPath, to.absolutePath
        )
        //Impacted targets
        cli.execute(
            "get-impacted-targets", "-sh", from.absolutePath, "-fh", to.absolutePath, "-o", impactedTargetsOutput.absolutePath
        )

        val actual: Set<String> = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
        val expected: Set<String> =
            javaClass.getResourceAsStream("/fixture/impacted_targets-1-2.txt").use { it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet() }

        assertThat(actual).isEqualTo(expected)
    }

    private fun extractFixtureProject(path: String): File {
        val testProject = temp.newFolder()
        val fixtureCopy = temp.newFile()

        fixtureCopy.outputStream().use {
            javaClass.getResourceAsStream(path).copyTo(it)
        }
        val zipFile = ZipFile(fixtureCopy)
        zipFile.stream().use {
            it.forEach { entry ->
                when {
                    entry.isDirectory -> {
                        Paths.get(testProject.absolutePath, entry.name).toFile().mkdirs()
                    }
                    else -> {
                        File(testProject, entry.name).apply {
                            parentFile.mkdir()
                            createNewFile()
                        }.outputStream().use { outputStream ->
                            zipFile.getInputStream(entry).copyTo(outputStream)
                        }
                    }
                }
            }
        }

        return testProject
    }
}
