package com.bazel_diff.e2e

import assertk.assertThat
import assertk.assertions.isEqualTo
import com.bazel_diff.cli.BazelDiff
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import picocli.CommandLine
import java.io.File
import java.nio.file.Paths
import java.util.zip.ZipFile


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

    @Test
    fun testFineGrainedHashExternalRepo() {
        // The difference between these two snapshots is simply upgrading the Guava version.
        // Following is the diff.
        //
        //   diff --git a/integration/WORKSPACE b/integration/WORKSPACE
        //   index 617a8d6..2cb3c7d 100644
        //   --- a/integration/WORKSPACE
        //   +++ b/integration/WORKSPACE
        //   @@ -15,7 +15,7 @@ maven_install(
        //        name = "bazel_diff_maven",
        //        artifacts = [
        //          "junit:junit:4.12",
        //   -      "com.google.guava:guava:31.0-jre",
        //   +      "com.google.guava:guava:31.1-jre",
        //        ],
        //        repositories = [
        //            "http://uk.maven.org/maven2",
        //
        // The project contains a single target that depends on Guava:
        // //src/main/java/com/integration:guava-user
        //
        // So this target, its derived targets, and all other changed external targets should be
        // the only impacted targets.
        val projectA = extractFixtureProject("/fixture/fine-grained-hash-external-repo-test-1.zip")
        val projectB = extractFixtureProject("/fixture/fine-grained-hash-external-repo-test-2.zip")

        val workingDirectoryA = projectA
        val workingDirectoryB = projectB
        val bazelPath = "bazel"
        val outputDir = temp.newFolder()
        val from = File(outputDir, "starting_hashes.json")
        val to = File(outputDir, "final_hashes.json")
        val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

        val cli = CommandLine(BazelDiff())
        //From
        cli.execute(
            "generate-hashes", "-w", workingDirectoryA.absolutePath, "-b", bazelPath, "--fineGrainedHashExternalRepos", "bazel_diff_maven", from.absolutePath
        )
        //To
        cli.execute(
            "generate-hashes", "-w", workingDirectoryB.absolutePath, "-b", bazelPath, "--fineGrainedHashExternalRepos", "bazel_diff_maven", to.absolutePath
        )
        //Impacted targets
        cli.execute(
            "get-impacted-targets", "-sh", from.absolutePath, "-fh", to.absolutePath, "-o", impactedTargetsOutput.absolutePath
        )

        val actual: Set<String> = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
        val expected: Set<String> =
            javaClass.getResourceAsStream("/fixture/fine-grained-hash-external-repo-test-impacted-targets.txt").use { it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet() }

        assertThat(actual).isEqualTo(expected)
    }

    @Test
    fun testUseCqueryWithExternalDependencyChange() {
        // The difference between these two snapshots is simply upgrading the Guava version for Android platform.
        // Following is the diff.
        //
        // diff --git a/WORKSPACE b/WORKSPACE
        // index 0fa6bdc..378ba11 100644
        // --- a/WORKSPACE
        // +++ b/WORKSPACE
        // @@ -27,7 +27,7 @@ maven_install(
        //      name = "bazel_diff_maven_android",
        //      artifacts = [
        //        "junit:junit:4.12",
        // -      "com.google.guava:guava:31.0-android",
        // +      "com.google.guava:guava:32.0.0-android",
        //      ],
        //      repositories = [
        //          "http://uk.maven.org/maven2",
        //
        // The project contains the following targets related to the test
        //
        // java_library(
        //     name = "guava-user",
        //     srcs = ["GuavaUser.java"] + select({
        //         "//:android_system": ["GuavaUserAndroid.java"],
        //         "//:jre_system": ["GuavaUserJre.java"],
        //     }),
        //     visibility = ["//visibility:public"],
        //     deps = select({
        //         "//:android_system": ["@bazel_diff_maven_android//:com_google_guava_guava"],
        //         "//:jre_system": ["@bazel_diff_maven//:com_google_guava_guava"],
        //     }),
        // )

        //   java_binary(
        //       name = "android",
        //       main_class = "cli.src.test.resources.integration.src.main.java.com.integration.GuavaUser",
        //       runtime_deps = ["guava-user"],
        //       target_compatible_with = ["//:android_system"]
        //   )

        //   java_binary(
        //       name = "jre",
        //       main_class = "cli.src.test.resources.integration.src.main.java.com.integration.GuavaUser",
        //       runtime_deps = ["guava-user"],
        //       target_compatible_with = ["//:jre_system"]
        //   )
        //
        // So with the above android upgrade, querying changed targets for the `jre` platform should not return anything
        // in the user repo changed. Querying changed targets for the `android` platform should only return `guava-user`
        // and `android` targets above because `jre` target above is not compatible with the `android` platform.

        val workingDirectoryA = extractFixtureProject("/fixture/cquery-test-base.zip")
        val workingDirectoryB = extractFixtureProject("/fixture/cquery-test-guava-upgrade.zip")
        val outputDir = temp.newFolder()
        val from = File(outputDir, "starting_hashes.json")
        val to = File(outputDir, "final_hashes.json")
        val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

        val cli = CommandLine(BazelDiff())
        // Query Android platform

        //From
        cli.execute(
            "generate-hashes", "-w", workingDirectoryA.absolutePath, "-b", "bazel", "--useCquery", "--cqueryCommandOptions", "--platforms=//:android", "--fineGrainedHashExternalRepos", "bazel_diff_maven,bazel_diff_maven_android", from.absolutePath
        )
        //To
        cli.execute(
            "generate-hashes", "-w", workingDirectoryB.absolutePath, "-b", "bazel", "--useCquery", "--cqueryCommandOptions", "--platforms=//:android", "--fineGrainedHashExternalRepos", "bazel_diff_maven,bazel_diff_maven_android", to.absolutePath
        )
        //Impacted targets
        cli.execute(
            "get-impacted-targets", "-sh", from.absolutePath, "-fh", to.absolutePath, "-o", impactedTargetsOutput.absolutePath
        )

        var actual: Set<String> = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
        var expected: Set<String> =
            javaClass.getResourceAsStream("/fixture/cquery-test-guava-upgrade-android-impacted-targets.txt").use { it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet() }

        assertThat(actual).isEqualTo(expected)

        // Query JRE platform

        //From
        cli.execute(
            "generate-hashes", "-w", workingDirectoryA.absolutePath, "-b", "bazel", "--useCquery", "--cqueryCommandOptions", "--platforms=//:jre", "--fineGrainedHashExternalRepos", "bazel_diff_maven,bazel_diff_maven_android", from.absolutePath
        )
        //To
        cli.execute(
            "generate-hashes", "-w", workingDirectoryB.absolutePath, "-b", "bazel", "--useCquery", "--cqueryCommandOptions", "--platforms=//:jre", "--fineGrainedHashExternalRepos", "bazel_diff_maven,bazel_diff_maven_android", to.absolutePath
        )
        //Impacted targets
        cli.execute(
            "get-impacted-targets", "-sh", from.absolutePath, "-fh", to.absolutePath, "-o", impactedTargetsOutput.absolutePath
        )

        actual = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
        expected = javaClass.getResourceAsStream("/fixture/cquery-test-guava-upgrade-jre-impacted-targets.txt").use { it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet() }

        assertThat(actual).isEqualTo(expected)
    }

    @Test
    fun testUseCqueryWithAndroidCodeChange() {
        // The difference between these two snapshots is simply making a code change to Android-only source code.
        // Following is the diff.
        //
        // diff --git a/src/main/java/com/integration/GuavaUserAndroid.java b/src/main/java/com/integration/GuavaUserAndroid.java
        // index 8a9289e..cb645dc 100644
        // --- a/src/main/java/com/integration/GuavaUserAndroid.java
        // +++ b/src/main/java/com/integration/GuavaUserAndroid.java
        // @@ -2,4 +2,6 @@ package cli.src.test.resources.integration.src.main.java.com.integration;
        //
        //  import com.google.common.collect.ImmutableList;
        //
        // -public class GuavaUserAndroid {}
        // +public class GuavaUserAndroid {
        // +  // add a comment
        // +}
        //
        // The project contains the following targets related to the test
        //
        // java_library(
        //     name = "guava-user",
        //     srcs = ["GuavaUser.java"] + select({
        //         "//:android_system": ["GuavaUserAndroid.java"],
        //         "//:jre_system": ["GuavaUserJre.java"],
        //     }),
        //     visibility = ["//visibility:public"],
        //     deps = select({
        //         "//:android_system": ["@bazel_diff_maven_android//:com_google_guava_guava"],
        //         "//:jre_system": ["@bazel_diff_maven//:com_google_guava_guava"],
        //     }),
        // )

        //   java_binary(
        //       name = "android",
        //       main_class = "cli.src.test.resources.integration.src.main.java.com.integration.GuavaUser",
        //       runtime_deps = ["guava-user"],
        //       target_compatible_with = ["//:android_system"]
        //   )

        //   java_binary(
        //       name = "jre",
        //       main_class = "cli.src.test.resources.integration.src.main.java.com.integration.GuavaUser",
        //       runtime_deps = ["guava-user"],
        //       target_compatible_with = ["//:jre_system"]
        //   )
        //
        // So with the above android code change, querying changed targets for the `jre` platform should not return
        // anything in the user repo changed. Querying changed targets for the `android` platform should only return
        // `guava-user` and `android` targets above because `jre` target above is not compatible with the `android`
        // platform.

        val workingDirectoryA = extractFixtureProject("/fixture/cquery-test-base.zip")
        val workingDirectoryB = extractFixtureProject("/fixture/cquery-test-android-code-change.zip")
        val outputDir = temp.newFolder()
        val from = File(outputDir, "starting_hashes.json")
        val to = File(outputDir, "final_hashes.json")
        val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

        val cli = CommandLine(BazelDiff())
        // Query Android platform

        //From
        cli.execute(
            "generate-hashes", "-w", workingDirectoryA.absolutePath, "-b", "bazel", "--useCquery", "--cqueryCommandOptions", "--platforms=//:android", "--fineGrainedHashExternalRepos", "bazel_diff_maven,bazel_diff_maven_android", from.absolutePath
        )
        //To
        cli.execute(
            "generate-hashes", "-w", workingDirectoryB.absolutePath, "-b", "bazel", "--useCquery", "--cqueryCommandOptions", "--platforms=//:android", "--fineGrainedHashExternalRepos", "bazel_diff_maven,bazel_diff_maven_android", to.absolutePath
        )
        //Impacted targets
        cli.execute(
            "get-impacted-targets", "-sh", from.absolutePath, "-fh", to.absolutePath, "-o", impactedTargetsOutput.absolutePath
        )

        var actual: Set<String> = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
        var expected: Set<String> =
            javaClass.getResourceAsStream("/fixture/cquery-test-android-code-change-android-impacted-targets.txt").use { it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet() }

        assertThat(actual).isEqualTo(expected)

        // Query JRE platform

        //From
        cli.execute(
            "generate-hashes", "-w", workingDirectoryA.absolutePath, "-b", "bazel", "--useCquery", "--cqueryCommandOptions", "--platforms=//:jre", "--fineGrainedHashExternalRepos", "bazel_diff_maven,bazel_diff_maven_android", from.absolutePath
        )
        //To
        cli.execute(
            "generate-hashes", "-w", workingDirectoryB.absolutePath, "-b", "bazel", "--useCquery", "--cqueryCommandOptions", "--platforms=//:jre", "--fineGrainedHashExternalRepos", "bazel_diff_maven,bazel_diff_maven_android", to.absolutePath
        )
        //Impacted targets
        cli.execute(
            "get-impacted-targets", "-sh", from.absolutePath, "-fh", to.absolutePath, "-o", impactedTargetsOutput.absolutePath
        )

        actual = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
        expected = javaClass.getResourceAsStream("/fixture/cquery-test-android-code-change-jre-impacted-targets.txt").use { it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet() }

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
