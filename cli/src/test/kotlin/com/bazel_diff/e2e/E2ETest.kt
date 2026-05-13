package com.bazel_diff.e2e

import assertk.assertThat
import assertk.assertions.isEqualTo
import com.bazel_diff.cli.BazelDiff
import com.google.gson.Gson
import com.google.gson.reflect.TypeToken
import java.io.File
import java.nio.file.Paths
import java.util.zip.ZipFile
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import picocli.CommandLine

class E2ETest {
  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  private fun CommandLine.execute(args: List<String>) = execute(*args.toTypedArray())

  private fun filterBazelDiffInternalTargets(targets: Set<String>): Set<String> {
    return targets.filter { target ->
      // Filter out bazel-diff's own internal test targets
      !target.contains("bazel-diff-integration-test") &&
      !target.contains("@@//:BUILD") &&
      !target.contains("bazel_diff_maven") && // Filter out bazel-diff's maven dependencies
      // Filter out platform-specific Maven alias targets that may or may not appear in cquery
      // results depending on Bazel version and platform (macOS vs Linux)
      !target.matches(Regex(".*rules_jvm_external\\+\\+maven\\+maven//:com_google_code_findbugs_jsr305$")) &&
      !target.matches(Regex(".*rules_jvm_external\\+\\+maven\\+maven//:com_google_guava_failureaccess$")) &&
      !target.matches(Regex(".*rules_jvm_external\\+\\+maven\\+maven//:com_google_guava_listenablefuture$")) &&
      !target.matches(Regex(".*rules_jvm_external\\+//private/tools/java/com/github/bazelbuild/rules_jvm_external/jar:AddJarManifestEntry$")) &&
      // Filter out junit and hamcrest which may appear on some platforms
      !target.matches(Regex(".*rules_jvm_external\\+\\+maven\\+maven//:junit_junit$")) &&
      !target.matches(Regex(".*rules_jvm_external\\+\\+maven\\+maven//:org_hamcrest_hamcrest_core$"))
    }.toSet()
  }

  private fun assertTargetsMatch(actual: Set<String>, expected: Set<String>, testContext: String = "") {
    if (actual != expected) {
      val missingTargets = expected - actual
      val unexpectedTargets = actual - expected

      val debugMessage = buildString {
        appendLine("\n========================================")
        appendLine("Target list mismatch${if (testContext.isNotEmpty()) " in $testContext" else ""}")
        appendLine("========================================")

        if (missingTargets.isNotEmpty()) {
          appendLine("\nMISSING TARGETS (expected but not found):")
          missingTargets.sorted().forEach { appendLine("  - $it") }
        }

        if (unexpectedTargets.isNotEmpty()) {
          appendLine("\nUNEXPECTED TARGETS (found but not expected):")
          unexpectedTargets.sorted().forEach { appendLine("  + $it") }
        }

        appendLine("\nEXPECTED (${expected.size} targets):")
        expected.sorted().forEach { appendLine("  $it") }

        appendLine("\nACTUAL (${actual.size} targets):")
        actual.sorted().forEach { appendLine("  $it") }
        appendLine("========================================")
      }

      println(debugMessage)
    }

    assertThat(actual).isEqualTo(expected)
  }

  private fun testE2E(
      extraGenerateHashesArgs: List<String>,
      extraGetImpactedTargetsArgs: List<String>,
      expectedResultFile: String
  ) {
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
    // From
    cli.execute(
        listOf(
            "generate-hashes",
            "-w",
            workingDirectoryA.absolutePath,
            "-b",
            bazelPath,
            from.absolutePath) + extraGenerateHashesArgs)
    // To
    cli.execute(
        listOf(
            "generate-hashes",
            "-w",
            workingDirectoryB.absolutePath,
            "-b",
            bazelPath,
            to.absolutePath) + extraGenerateHashesArgs)
    // Impacted targets
    cli.execute(
        listOf(
            "get-impacted-targets",
            "-w",
            workingDirectoryB.absolutePath,
            "-b",
            bazelPath,
            "-sh",
            from.absolutePath,
            "-fh",
            to.absolutePath,
            "-o",
            impactedTargetsOutput.absolutePath) + extraGetImpactedTargetsArgs)

    val actual: Set<String> = filterBazelDiffInternalTargets(
        impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    val expected: Set<String> =
        javaClass.getResourceAsStream(expectedResultFile).use {
          filterBazelDiffInternalTargets(
              it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet())
        }

    assertTargetsMatch(actual, expected, "testE2E")
  }

  @Test
  fun testE2E() {
    testE2E(emptyList(), emptyList(), "/fixture/impacted_targets-1-2.txt")
  }

  @Test
  fun testDetermineBazelVersion() {
    // E2E coverage for BazelQueryService.determineBazelVersion(): version is resolved lazily
    // when the first query runs. Running generate-hashes to completion validates that
    // "bazel version" is executed and parsed successfully (e.g. "Build label: X.Y.Z").
    val projectA = extractFixtureProject("/fixture/integration-test-1.zip")
    val workingDirectory = File(projectA, "integration")
    val outputDir = temp.newFolder()
    val outputPath = File(outputDir, "hashes.json")

    val cli = CommandLine(BazelDiff())
    val exitCode = cli.execute(
        "generate-hashes",
        "-w",
        workingDirectory.absolutePath,
        "-b",
        "bazel",
        outputPath.absolutePath)

    assertThat(exitCode).isEqualTo(0)
    assertThat(outputPath.readText().isNotEmpty()).isEqualTo(true)
  }

  @Test
  fun testE2EWithNoKeepGoing() {
    testE2E(
        listOf("--no-keep_going"),
        emptyList(),
        "/fixture/impacted_targets-1-2.txt")
  }

  @Test
  fun testE2EIncludingTargetType() {
    testE2E(
        listOf("-tt", "Rule,SourceFile"),
        emptyList(),
        "/fixture/impacted_targets-1-2-rule-sourcefile.txt")
  }

  @Test
  fun testE2EWithTargetType() {
    testE2E(
        listOf("--includeTargetType"),
        listOf("-tt", "Rule,SourceFile"),
        "/fixture/impacted_targets-1-2-rule-sourcefile.txt")
  }

  @Test
  fun testGenerateHashesWithCqueryStreamedProto() {
    // Validates the --useCquery code path that consumes Bazel's `--output=streamed_proto`
    // cquery output (https://github.com/Tinder/bazel-diff/issues/219). On Bazel 7.0.0+ this
    // exercises the AnalysisProtosV2.CqueryResult.parseDelimitedFrom loop in
    // BazelQueryService.query(); on older Bazel it falls back to single-message proto parsing.
    // Uses the `distance_metrics` shell-only workspace so cquery analysis works without a
    // sandboxed JDK / coursier fetch.
    val workspace = copyTestWorkspace("distance_metrics")
    val outputDir = temp.newFolder()
    val output = File(outputDir, "hashes.json")

    val cli = CommandLine(BazelDiff())
    val exitCode = cli.execute(
        "generate-hashes",
        "-w", workspace.absolutePath,
        "-b", "bazel",
        "--useCquery",
        output.absolutePath)

    assertThat(exitCode).isEqualTo(0)

    val hashes: Map<String, Any> =
        Gson().fromJson(output.readText(), object : TypeToken<Map<String, Any>>() {}.type)
    assertThat(hashes.isNotEmpty()).isEqualTo(true)
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
    // From
    cli.execute(
        "generate-hashes",
        "-w",
        workingDirectoryA.absolutePath,
        "-b",
        bazelPath,
        "--fineGrainedHashExternalRepos",
        "@bazel_diff_maven",
        from.absolutePath)
    // To
    cli.execute(
        "generate-hashes",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        bazelPath,
        "--fineGrainedHashExternalRepos",
        "@bazel_diff_maven",
        to.absolutePath)
    // Impacted targets
    cli.execute(
        "get-impacted-targets",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        bazelPath,
        "-sh",
        from.absolutePath,
        "-fh",
        to.absolutePath,
        "-o",
        impactedTargetsOutput.absolutePath)

    val actual: Set<String> = filterBazelDiffInternalTargets(
        impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    val expected: Set<String> =
        javaClass
            .getResourceAsStream(
                "/fixture/fine-grained-hash-external-repo-test-impacted-targets.txt")
            .use { filterBazelDiffInternalTargets(
                it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet()) }

    assertTargetsMatch(actual, expected, "testFineGrainedHashExternalRepo")
  }

  private fun testFineGrainedHashBzlMod(
      extraGenerateHashesArgs: List<String>,
      fineGrainedHashExternalRepo: String,
      expectedResultFile: String
  ) {
    // The difference between these two snapshots is simply upgrading the Guava version.
    // Following is the diff. (The diff on maven_install.json is omitted)
    //
    // diff --git a/MODULE.bazel b/MODULE.bazel
    //         index 9a58823..3ffded3 100644
    // --- a/MODULE.bazel
    // +++ b/MODULE.bazel
    // @@ -4,7 +4,7 @@ maven = use_extension("@rules_jvm_external//:extensions.bzl", "maven")
    // maven.install(
    //         artifacts = [
    //             "junit:junit:4.12",
    //             -        "com.google.guava:guava:31.1-jre",
    //             +        "com.google.guava:guava:32.0.0-jre",
    //         ],
    //         lock_file = "//:maven_install.json",
    //         repositories = [
    //
    // The project contains a single target that depends on Guava:
    // //src/main/java/com/integration:guava-user
    //
    // So this target, its derived targets, and all other changed external targets should be
    // the only impacted targets.
    val projectA = extractFixtureProject("/fixture/fine-grained-hash-bzlmod-test-1.zip")
    val projectB = extractFixtureProject("/fixture/fine-grained-hash-bzlmod-test-2.zip")

    val workingDirectoryA = projectA
    val workingDirectoryB = projectB
    val bazelPath = "bazel"
    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())
    // From
    cli.execute(
        listOf(
            "generate-hashes",
            "-w",
            workingDirectoryA.absolutePath,
            "-b",
            bazelPath,
            "--fineGrainedHashExternalRepos",
            fineGrainedHashExternalRepo,
            from.absolutePath) + extraGenerateHashesArgs)
    // To
    cli.execute(
        listOf(
            "generate-hashes",
            "-w",
            workingDirectoryB.absolutePath,
            "-b",
            bazelPath,
            "--fineGrainedHashExternalRepos",
            fineGrainedHashExternalRepo,
            to.absolutePath) + extraGenerateHashesArgs)
    // Impacted targets
    cli.execute(
        "get-impacted-targets",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        bazelPath,
        "-sh",
        from.absolutePath,
        "-fh",
        to.absolutePath,
        "-o",
        impactedTargetsOutput.absolutePath)

    val actual: Set<String> = filterBazelDiffInternalTargets(
        impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    val expected: Set<String> =
        javaClass.getResourceAsStream(expectedResultFile).use {
          filterBazelDiffInternalTargets(
              it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet())
        }

    assertTargetsMatch(actual, expected, "testFineGrainedHashBzlMod")
  }

  @Test
  fun testFineGrainedHashBzlMod() {
    testFineGrainedHashBzlMod(
        emptyList(),
        "@bazel_diff_maven",
        "/fixture/fine-grained-hash-bzlmod-test-impacted-targets.txt")
  }

  @Test
  fun testFineGrainedHashBzlModCquery() {
    testFineGrainedHashBzlMod(
        listOf("--useCquery"),
        "@@rules_jvm_external~~maven~maven",
        "/fixture/fine-grained-hash-bzlmod-cquery-test-impacted-targets.txt")
  }

  private fun testBzlmodTransitiveDeps(
      extraGenerateHashesArgs: List<String>,
      fineGrainedHashExternalRepo: String,
      expectedResultFile: String
  ) {
    // This test validates that transitive dependencies are properly tracked when bzlmod external
    // dependencies change.
    //
    // The fixtures contain:
    //   - target-a: depends on target-b (transitive dependency on Guava)
    //   - target-b: directly depends on Guava external library
    //
    // When Guava version changes (31.1-jre -> 32.0.0-jre), BOTH targets should be impacted:
    //   - target-b is directly impacted (it uses Guava)
    //   - target-a is transitively impacted (it depends on target-b which depends on Guava)
    //
    // This test reproduces the issue reported in https://github.com/Tinder/bazel-diff/issues/293
    // where transitive dependencies may not be properly detected with bzlmod.
    val projectA = extractFixtureProject("/fixture/bzlmod-transitive-test-1.zip")
    val projectB = extractFixtureProject("/fixture/bzlmod-transitive-test-2.zip")

    val workingDirectoryA = projectA
    val workingDirectoryB = projectB
    val bazelPath = "bazel"
    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())
    // From
    cli.execute(
        listOf(
            "generate-hashes",
            "-w",
            workingDirectoryA.absolutePath,
            "-b",
            bazelPath,
            "--fineGrainedHashExternalRepos",
            fineGrainedHashExternalRepo,
            from.absolutePath) + extraGenerateHashesArgs)
    // To
    cli.execute(
        listOf(
            "generate-hashes",
            "-w",
            workingDirectoryB.absolutePath,
            "-b",
            bazelPath,
            "--fineGrainedHashExternalRepos",
            fineGrainedHashExternalRepo,
            to.absolutePath) + extraGenerateHashesArgs)
    // Impacted targets
    cli.execute(
        "get-impacted-targets",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        bazelPath,
        "-sh",
        from.absolutePath,
        "-fh",
        to.absolutePath,
        "-o",
        impactedTargetsOutput.absolutePath)

    val actual: Set<String> = filterBazelDiffInternalTargets(
        impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    val expected: Set<String> =
        javaClass.getResourceAsStream(expectedResultFile).use {
          filterBazelDiffInternalTargets(
              it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet())
        }

    assertTargetsMatch(actual, expected, "testBzlmodTransitiveDeps")
  }

  @Test
  fun testBzlmodTransitiveDepsQuery() {
    testBzlmodTransitiveDeps(
        emptyList(),
        "@bazel_diff_maven",
        "/fixture/bzlmod-transitive-test-impacted-targets.txt")
  }

  @Test
  fun testBzlmodTransitiveDepsCquery() {
    testBzlmodTransitiveDeps(
        listOf("--useCquery"),
        "@@rules_jvm_external~~maven~maven",
        "/fixture/bzlmod-transitive-test-cquery-impacted-targets.txt")
  }

  private fun testBzlmodCCTransitiveDeps(
      extraGenerateHashesArgs: List<String>,
      expectedResultFile: String
  ) {
    // This test validates transitive dependency tracking for native C++ libraries when
    // Bazel module versions change in MODULE.bazel.
    //
    // The fixtures contain:
    //   - target-a (cc_library): depends on target-b
    //   - target-b (cc_library): directly depends on abseil-cpp external module
    //
    // When abseil-cpp version changes (20240116.2 -> 20240722.0), bazel-diff now detects
    // these changes via module graph hashing (implemented in BazelModService.getModuleGraph()).
    //
    // Expected behavior:
    //   - target-b is impacted (uses abseil directly)
    //   - target-a is transitively impacted (depends on target-b)
    //   - All targets are invalidated because the module graph is included in the seed hash
    //
    // This validates the fix for:
    // https://github.com/Tinder/bazel-diff/issues/293
    val projectA = extractFixtureProject("/fixture/bzlmod-cc-transitive-test-1.zip")
    val projectB = extractFixtureProject("/fixture/bzlmod-cc-transitive-test-2.zip")

    val workingDirectoryA = projectA
    val workingDirectoryB = projectB
    val bazelPath = "bazel"
    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())
    // From
    cli.execute(
        listOf(
            "generate-hashes",
            "-w",
            workingDirectoryA.absolutePath,
            "-b",
            bazelPath,
            from.absolutePath) + extraGenerateHashesArgs)
    // To
    cli.execute(
        listOf(
            "generate-hashes",
            "-w",
            workingDirectoryB.absolutePath,
            "-b",
            bazelPath,
            to.absolutePath) + extraGenerateHashesArgs)
    // Impacted targets
    cli.execute(
        "get-impacted-targets",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        bazelPath,
        "-sh",
        from.absolutePath,
        "-fh",
        to.absolutePath,
        "-o",
        impactedTargetsOutput.absolutePath)

    val actual: Set<String> = filterBazelDiffInternalTargets(
        impactedTargetsOutput.readLines().filter { it.isNotBlank() && !it.startsWith("#") }.toSet())
    val expected: Set<String> =
        javaClass.getResourceAsStream(expectedResultFile).use {
          filterBazelDiffInternalTargets(
              it.bufferedReader().readLines().filter { it.isNotBlank() && !it.startsWith("#") }.toSet())
        }

    assertTargetsMatch(actual, expected, "testBzlmodCCTransitiveDeps")
  }

  @Test
  @org.junit.Ignore("Skipped due to Bazel version compatibility issues in test environment. " +
      "The fixtures use Bazel 7.0.0 but test environment uses Bazel 8+, causing bzlmod/abseil-cpp " +
      "package loading errors. This test is ready to run when Bazel version handling is resolved. " +
      "Module graph hashing has been implemented and should detect transitive dependency changes.")
  fun testBzlmodCCTransitiveDepsQuery() {
    // This test validates that MODULE.bazel changes are now detected via module graph hashing.
    // Both target-a and target-b should be impacted when abseil-cpp version changes.
    testBzlmodCCTransitiveDeps(
        emptyList(),
        "/fixture/bzlmod-cc-transitive-test-impacted-targets.txt")
  }

  @Test
  fun testUseCqueryWithExternalDependencyChange() {
    // The difference between these two snapshots is simply upgrading the Guava version for Android
    // platform.
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
    //       main_class =
    // "cli.src.test.resources.integration.src.main.java.com.integration.GuavaUser",
    //       runtime_deps = ["guava-user"],
    //       target_compatible_with = ["//:android_system"]
    //   )

    //   java_binary(
    //       name = "jre",
    //       main_class =
    // "cli.src.test.resources.integration.src.main.java.com.integration.GuavaUser",
    //       runtime_deps = ["guava-user"],
    //       target_compatible_with = ["//:jre_system"]
    //   )
    //
    // So with the above android upgrade, querying changed targets for the `jre` platform should not
    // return anything
    // in the user repo changed. Querying changed targets for the `android` platform should only
    // return `guava-user`
    // and `android` targets above because `jre` target above is not compatible with the `android`
    // platform.

    val workingDirectoryA = extractFixtureProject("/fixture/cquery-test-base.zip")
    val workingDirectoryB = extractFixtureProject("/fixture/cquery-test-guava-upgrade.zip")
    val bazelPath = "bazel"
    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())
    // Query Android platform

    // From
    cli.execute(
        "generate-hashes",
        "-w",
        workingDirectoryA.absolutePath,
        "-b",
        "bazel",
        "--useCquery",
        "--cqueryCommandOptions",
        "--platforms=//:android",
        "--fineGrainedHashExternalRepos",
        "@@bazel_diff_maven,@@bazel_diff_maven_android",
        from.absolutePath)
    // To
    cli.execute(
        "generate-hashes",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        "bazel",
        "--useCquery",
        "--cqueryCommandOptions",
        "--platforms=//:android",
        "--fineGrainedHashExternalRepos",
        "@@bazel_diff_maven,@@bazel_diff_maven_android",
        to.absolutePath)
    // Impacted targets
    cli.execute(
        "get-impacted-targets",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        bazelPath,
        "-sh",
        from.absolutePath,
        "-fh",
        to.absolutePath,
        "-o",
        impactedTargetsOutput.absolutePath)

    var actual: Set<String> = filterBazelDiffInternalTargets(
        impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    var expected: Set<String> =
        javaClass
            .getResourceAsStream("/fixture/cquery-test-guava-upgrade-android-impacted-targets.txt")
            .use { filterBazelDiffInternalTargets(
                it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet()) }

    assertTargetsMatch(actual, expected, "testUseCqueryWithExternalDependencyChange - Android platform")

    // Query JRE platform

    // From
    cli.execute(
        "generate-hashes",
        "-w",
        workingDirectoryA.absolutePath,
        "-b",
        "bazel",
        "--useCquery",
        "--cqueryCommandOptions",
        "--platforms=//:jre",
        "--fineGrainedHashExternalRepos",
        "@@bazel_diff_maven,@@bazel_diff_maven_android",
        from.absolutePath)
    // To
    cli.execute(
        "generate-hashes",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        "bazel",
        "--useCquery",
        "--cqueryCommandOptions",
        "--platforms=//:jre",
        "--fineGrainedHashExternalRepos",
        "@@bazel_diff_maven,@@bazel_diff_maven_android",
        to.absolutePath)
    // Impacted targets
    cli.execute(
        "get-impacted-targets",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        bazelPath,
        "-sh",
        from.absolutePath,
        "-fh",
        to.absolutePath,
        "-o",
        impactedTargetsOutput.absolutePath)

    actual = filterBazelDiffInternalTargets(
        impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    expected =
        javaClass
            .getResourceAsStream("/fixture/cquery-test-guava-upgrade-jre-impacted-targets.txt")
            .use { filterBazelDiffInternalTargets(
                it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet()) }

    assertTargetsMatch(actual, expected, "testUseCqueryWithExternalDependencyChange - JRE platform")
  }

  @Test
  fun testUseCqueryWithAndroidCodeChange() {
    // The difference between these two snapshots is simply making a code change to Android-only
    // source code.
    // Following is the diff.
    //
    // diff --git a/src/main/java/com/integration/GuavaUserAndroid.java
    // b/src/main/java/com/integration/GuavaUserAndroid.java
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
    //       main_class =
    // "cli.src.test.resources.integration.src.main.java.com.integration.GuavaUser",
    //       runtime_deps = ["guava-user"],
    //       target_compatible_with = ["//:android_system"]
    //   )

    //   java_binary(
    //       name = "jre",
    //       main_class =
    // "cli.src.test.resources.integration.src.main.java.com.integration.GuavaUser",
    //       runtime_deps = ["guava-user"],
    //       target_compatible_with = ["//:jre_system"]
    //   )
    //
    // So with the above android code change, querying changed targets for the `jre` platform should
    // not return
    // anything in the user repo changed. Querying changed targets for the `android` platform should
    // only return
    // `guava-user` and `android` targets above because `jre` target above is not compatible with
    // the `android`
    // platform.

    val workingDirectoryA = extractFixtureProject("/fixture/cquery-test-base.zip")
    val workingDirectoryB = extractFixtureProject("/fixture/cquery-test-android-code-change.zip")
    val bazelPath = "bazel"
    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())
    // Query Android platform

    // From
    cli.execute(
        "generate-hashes",
        "-w",
        workingDirectoryA.absolutePath,
        "-b",
        "bazel",
        "--useCquery",
        "--cqueryCommandOptions",
        "--platforms=//:android",
        "--fineGrainedHashExternalRepos",
        "@@bazel_diff_maven,@@bazel_diff_maven_android",
        from.absolutePath)
    // To
    cli.execute(
        "generate-hashes",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        "bazel",
        "--useCquery",
        "--cqueryCommandOptions",
        "--platforms=//:android",
        "--fineGrainedHashExternalRepos",
        "@@bazel_diff_maven,@@bazel_diff_maven_android",
        to.absolutePath)
    // Impacted targets
    cli.execute(
        "get-impacted-targets",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        bazelPath,
        "-sh",
        from.absolutePath,
        "-fh",
        to.absolutePath,
        "-o",
        impactedTargetsOutput.absolutePath)

    var actual: Set<String> = filterBazelDiffInternalTargets(
        impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    var expected: Set<String> =
        javaClass
            .getResourceAsStream(
                "/fixture/cquery-test-android-code-change-android-impacted-targets.txt")
            .use { filterBazelDiffInternalTargets(
                it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet()) }

    assertTargetsMatch(actual, expected, "testUseCqueryWithAndroidCodeChange - Android platform")

    // Query JRE platform

    // From
    cli.execute(
        "generate-hashes",
        "-w",
        workingDirectoryA.absolutePath,
        "-b",
        "bazel",
        "--useCquery",
        "--cqueryCommandOptions",
        "--platforms=//:jre",
        "--fineGrainedHashExternalRepos",
        "@@bazel_diff_maven,@@bazel_diff_maven_android",
        from.absolutePath)
    // To
    cli.execute(
        "generate-hashes",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        "bazel",
        "--useCquery",
        "--cqueryCommandOptions",
        "--platforms=//:jre",
        "--fineGrainedHashExternalRepos",
        "@@bazel_diff_maven,@@bazel_diff_maven_android",
        to.absolutePath)
    // Impacted targets
    cli.execute(
        "get-impacted-targets",
        "-w",
        workingDirectoryB.absolutePath,
        "-b",
        bazelPath,
        "-sh",
        from.absolutePath,
        "-fh",
        to.absolutePath,
        "-o",
        impactedTargetsOutput.absolutePath)

    actual = filterBazelDiffInternalTargets(
        impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    expected =
        javaClass
            .getResourceAsStream(
                "/fixture/cquery-test-android-code-change-jre-impacted-targets.txt")
            .use { filterBazelDiffInternalTargets(
                it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet()) }

    assertTargetsMatch(actual, expected, "testUseCqueryWithAndroidCodeChange - JRE platform")
  }


  @Test
  fun testTargetDistanceMetrics() {
    val workspace = copyTestWorkspace("distance_metrics")

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val depsFile = File(outputDir, "depEdges.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())

    cli.execute(
        "generate-hashes",
        "--includeTargetType",
        "-w",
        workspace.absolutePath,
        "-b",
        "bazel",
        from.absolutePath)
    // Modify the workspace
    File(workspace, "A/one.sh").appendText("foo")
    cli.execute(
        "generate-hashes",
        "--includeTargetType",
        "-w",
        workspace.absolutePath,
        "-d",
        depsFile.absolutePath,
        "-b",
        "bazel",
        to.absolutePath)

    // Impacted targets
    cli.execute(
        "get-impacted-targets",
        "-w",
        workspace.absolutePath,
        "-b",
        "bazel",
        "-sh",
        from.absolutePath,
        "-fh",
        to.absolutePath,
        "-d",
        depsFile.absolutePath,
        "-tt",
        "Rule,GeneratedFile",
        "-o",
        impactedTargetsOutput.absolutePath,
    )

    val gson = Gson()
    val shape = object : TypeToken<List<Map<String, Any>>>() {}.type
    val actual =
        gson.fromJson<List<Map<String, Any>>>(impactedTargetsOutput.readText(), shape)
            .filter { target ->
              // Filter out Bazel convenience symlink targets (bazel-*) as they're not reliably
              // present across all environments
              !(target["label"] as String).contains("//bazel-")
            }
            .sortedBy { it["label"] as String }
    val expected: List<Map<String, Any>> =
        listOf(
            mapOf("label" to "//A:one", "targetDistance" to 0.0, "packageDistance" to 0.0),
            mapOf("label" to "//A:gen_two", "targetDistance" to 1.0, "packageDistance" to 0.0),
            mapOf("label" to "//A:two.sh", "targetDistance" to 2.0, "packageDistance" to 0.0),
            mapOf("label" to "//A:two", "targetDistance" to 3.0, "packageDistance" to 0.0),
            mapOf("label" to "//A:three", "targetDistance" to 4.0, "packageDistance" to 0.0),
            mapOf("label" to "//:lib", "targetDistance" to 5.0, "packageDistance" to 1.0))

    assertThat(actual.size).isEqualTo(expected.size)

    expected.forEach { expectedMap ->
      val actualMap = actual.find { it["label"] == expectedMap["label"] }
      assertThat(actualMap).isEqualTo(expectedMap)
    }
  }

  @Test
  fun testCqueryWithFailingAnalysisTargets() {
    // Reproducer for https://github.com/Tinder/bazel-diff/issues/301
    // This test demonstrates the issue where cquery executes implementation functions
    // for all repository targets, causing targets designed to fail (like analysis_test
    // from rules_testing) to fail during cquery execution.
    //
    // The workspace contains:
    // - normal_target: A regular target that works fine
    // - dependent_target: Another regular target
    // - failing_analysis_target: A target designed to fail during analysis
    //
    // Expected behavior:
    // - With query: All targets are found without executing implementation functions
    // - With cquery: The failing_analysis_target causes analysis to fail
    // - With cquery + keep_going: Partial results should be returned (only the non-failing targets)
    //
    // This test verifies the current behavior and demonstrates the issue.

    val workspace = copyTestWorkspace("cquery_failing_target")
    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")

    val cli = CommandLine(BazelDiff())

    // First, verify that generate-hashes works without --useCquery
    val exitCodeWithoutCquery = cli.execute(
        "generate-hashes",
        "-w",
        workspace.absolutePath,
        "-b",
        "bazel",
        from.absolutePath)

    assertThat(exitCodeWithoutCquery).isEqualTo(0)

    // Now, verify that generate-hashes fails with --useCquery due to the failing target
    // This demonstrates the issue described in #301
    val exitCodeWithCquery = cli.execute(
        "generate-hashes",
        "-w",
        workspace.absolutePath,
        "-b",
        "bazel",
        "--useCquery",
        from.absolutePath)

    // The cquery should fail because it tries to analyze the failing_analysis_target
    // which is designed to fail during analysis
    assertThat(exitCodeWithCquery).isEqualTo(1)

    // Test with --no-keep_going: cquery should fail (no partial results, immediate failure)
    val outputNoKeepGoing = File(outputDir, "hashes_no_keep_going.json")
    val exitCodeWithNoKeepGoing = cli.execute(
        "generate-hashes",
        "-w",
        workspace.absolutePath,
        "-b",
        "bazel",
        "--useCquery",
        "--no-keep_going",
        outputNoKeepGoing.absolutePath)
    assertThat(exitCodeWithNoKeepGoing).isEqualTo(1)

    // Test with --keep_going enabled (default behavior)
    // With keep_going, cquery returns partial results but still exits with code 1
    // The current implementation allows exit codes 0 and 3, but cquery with keep_going
    // returns exit code 1 when some targets fail analysis
    val exitCodeWithCqueryKeepGoing = cli.execute(
        "generate-hashes",
        "-w",
        workspace.absolutePath,
        "-b",
        "bazel",
        "--useCquery",
        "--keep_going",
        from.absolutePath)

    // This currently fails (exit code 1) because bazel-diff only allows exit codes 0 and 3
    // but cquery with --keep_going returns exit code 1 when partial results are available
    assertThat(exitCodeWithCqueryKeepGoing).isEqualTo(1)

    // Test with custom cquery expression to exclude the failing target
    // Note: We use explicit target selection instead of wildcard + except because
    // cquery analyzes targets during pattern expansion, so "//...:all-targets except X"
    // would still try to analyze X. The solution is to explicitly specify which targets to query.
    val exitCodeWithCustomExpression = cli.execute(
        "generate-hashes",
        "-w",
        workspace.absolutePath,
        "-b",
        "bazel",
        "--useCquery",
        "--cqueryExpression",
        "deps(//:normal_target) + deps(//:dependent_target)",
        from.absolutePath)

    // With the custom expression that explicitly lists only the non-failing targets, this should succeed
    assertThat(exitCodeWithCustomExpression).isEqualTo(0)

    // Verify the hashes were generated successfully and contain the expected targets
    val hashes = from.readText()
    assertThat(hashes.contains("normal_target")).isEqualTo(true)
    assertThat(hashes.contains("dependent_target")).isEqualTo(true)
    // The failing target should not be in the hashes since it wasn't included in the query
    assertThat(hashes.contains("failing_analysis_target")).isEqualTo(false)
  }

  /**
   * Returns the Bazel version triple by running `bazel version`, or null if it cannot be determined.
   */
  private fun getBazelVersion(): Triple<Int, Int, Int>? {
    return try {
      val process = ProcessBuilder("bazel", "version")
          .redirectErrorStream(true)
          .start()
      val output = process.inputStream.bufferedReader().readText()
      process.waitFor()
      val versionLine = output.lines().firstOrNull { it.startsWith("Build label: ") }
          ?.removePrefix("Build label: ")?.trim() ?: return null
      val parts = versionLine.split('-')[0].split('.').map { it.takeWhile { c -> c.isDigit() }.toInt() }
      Triple(parts[0], parts[1], parts[2])
    } catch (_: Exception) {
      null
    }
  }

  @Test
  fun testBzlmodShowRepoDetectsModuleBazelChanges() {
    // Validates the fix for https://github.com/Tinder/bazel-diff/issues/255
    //
    // When MODULE.bazel changes a dependency version (e.g. guava 31.1-jre -> 32.0.0-jre),
    // bazel-diff should detect the change via `bazel mod show_repo --output=streamed_proto`
    // and report only the affected targets — WITHOUT requiring --fineGrainedHashExternalRepos.
    //
    // This test requires Bazel 8.6.0+ (or 9.0.1+) which supports the `mod show_repo`
    // streamed_proto output. It is skipped on older Bazel versions.
    val version = getBazelVersion()
    org.junit.Assume.assumeNotNull(version)
    val v = version!!
    val comparator = compareBy<Triple<Int, Int, Int>> { it.first }.thenBy { it.second }.thenBy { it.third }
    val hasModShowRepo = comparator.compare(v, Triple(8, 6, 0)) >= 0 && v != Triple(9, 0, 0)
    org.junit.Assume.assumeTrue(
        "Requires Bazel 8.6.0+ or 9.0.1+ (current: ${v.first}.${v.second}.${v.third})",
        hasModShowRepo)

    val projectA = extractFixtureProject("/fixture/bzlmod-show-repo-test-1.zip")
    val projectB = extractFixtureProject("/fixture/bzlmod-show-repo-test-2.zip")

    val bazelPath = "bazel"
    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())

    // Generate hashes for both snapshots WITHOUT --fineGrainedHashExternalRepos.
    // The mod show_repo integration should still detect the MODULE.bazel change.
    val exitFrom = cli.execute(
        "generate-hashes",
        "-w", projectA.absolutePath,
        "-b", bazelPath,
        from.absolutePath)
    assertThat(exitFrom).isEqualTo(0)

    val exitTo = cli.execute(
        "generate-hashes",
        "-w", projectB.absolutePath,
        "-b", bazelPath,
        to.absolutePath)
    assertThat(exitTo).isEqualTo(0)

    // Get impacted targets
    cli.execute(
        "get-impacted-targets",
        "-w", projectB.absolutePath,
        "-b", bazelPath,
        "-sh", from.absolutePath,
        "-fh", to.absolutePath,
        "-o", impactedTargetsOutput.absolutePath)

    val impactedTargets = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()

    // guava-user depends on an external maven artifact (guava), so it must be impacted
    // when the guava version changes in MODULE.bazel.
    val guavaUserImpacted = impactedTargets.any { it.contains("guava-user") }
    assertThat(guavaUserImpacted)
        .transform("guava-user should be in impacted targets: $impactedTargets") { it }
        .isEqualTo(true)

    // bazel-diff-integration-lib depends only on local targets (Submodule), NOT on any
    // external maven artifact. It should NOT be impacted by the guava version change.
    val integrationLibImpacted = impactedTargets.any {
      it.contains("bazel-diff-integration-lib") && !it.contains("libbazel-diff-integration-lib")
    }
    assertThat(integrationLibImpacted)
        .transform("bazel-diff-integration-lib should NOT be in impacted targets: $impactedTargets") { it }
        .isEqualTo(false)

    // Submodule has no external deps at all - it should not be impacted.
    val submoduleImpacted = impactedTargets.any { it.contains("submodule") }
    assertThat(submoduleImpacted)
        .transform("Submodule should NOT be in impacted targets: $impactedTargets") { it }
        .isEqualTo(false)
  }

  @Test
  fun testExcludeExternalTargetsFiltersBzlmodSyntheticLabels() {
    // Validates the fix for https://github.com/Tinder/bazel-diff/issues/326.
    //
    // On Bazel 8.6.0+ with bzlmod, BazelClient queries `bazel mod show_repo` to produce synthetic
    // //external:<apparent_name> targets so generate-hashes can detect bzlmod dep changes. Those
    // labels are unbuildable in bzlmod-only mode (no //external package), so users hit
    // "no such package 'external'" when feeding the impacted-targets file to `bazel build`.
    //
    // Expected behavior:
    //   - get-impacted-targets defaults --excludeExternalTargets to TRUE when bzlmod is detected
    //     => no //external:* lines in the output.
    //   - --no-excludeExternalTargets opts back into the legacy behavior so the synthetic labels
    //     reappear (proving they're really in the hash maps and the filter is what removes them).
    val version = getBazelVersion()
    org.junit.Assume.assumeNotNull(version)
    val v = version!!
    val comparator = compareBy<Triple<Int, Int, Int>> { it.first }.thenBy { it.second }.thenBy { it.third }
    val hasModShowRepo = comparator.compare(v, Triple(8, 6, 0)) >= 0 && v != Triple(9, 0, 0)
    org.junit.Assume.assumeTrue(
        "Requires Bazel 8.6.0+ or 9.0.1+ (current: ${v.first}.${v.second}.${v.third})",
        hasModShowRepo)

    val projectA = extractFixtureProject("/fixture/bzlmod-show-repo-test-1.zip")
    val projectB = extractFixtureProject("/fixture/bzlmod-show-repo-test-2.zip")

    val bazelPath = "bazel"
    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val defaultOutput = File(outputDir, "impacted_default.txt")
    val optOutOutput = File(outputDir, "impacted_optout.txt")

    val cli = CommandLine(BazelDiff())

    assertThat(
        cli.execute(
            "generate-hashes",
            "-w", projectA.absolutePath,
            "-b", bazelPath,
            from.absolutePath))
        .isEqualTo(0)
    assertThat(
        cli.execute(
            "generate-hashes",
            "-w", projectB.absolutePath,
            "-b", bazelPath,
            to.absolutePath))
        .isEqualTo(0)

    // Default invocation: bzlmod is detected, so --excludeExternalTargets auto-defaults to true.
    assertThat(
        cli.execute(
            "get-impacted-targets",
            "-w", projectB.absolutePath,
            "-b", bazelPath,
            "-sh", from.absolutePath,
            "-fh", to.absolutePath,
            "-o", defaultOutput.absolutePath))
        .isEqualTo(0)

    val defaultLines = defaultOutput.readLines().filter { it.isNotBlank() }
    val leakedExternal = defaultLines.filter { it.startsWith("//external:") }
    assertThat(leakedExternal.isEmpty())
        .transform(
            "default impacted-targets output should not contain //external:* labels, but found: $leakedExternal") { it }
        .isEqualTo(true)

    // Opt-out: --no-excludeExternalTargets reproduces the pre-#326 behavior so the synthetic
    // labels show up. This proves the labels really exist in the hashes (so the filter is doing
    // real work) and gives users an escape hatch.
    assertThat(
        cli.execute(
            "get-impacted-targets",
            "-w", projectB.absolutePath,
            "-b", bazelPath,
            "-sh", from.absolutePath,
            "-fh", to.absolutePath,
            "--no-excludeExternalTargets",
            "-o", optOutOutput.absolutePath))
        .isEqualTo(0)

    val optOutLines = optOutOutput.readLines().filter { it.isNotBlank() }
    val externalsWithOptOut = optOutLines.filter { it.startsWith("//external:") }
    assertThat(externalsWithOptOut.isNotEmpty())
        .transform(
            "with --no-excludeExternalTargets, the impacted-targets output should expose synthetic //external:* labels (none found in: $optOutLines)") { it }
        .isEqualTo(true)
  }

  // ------------------------------------------------------------------------
  // Regression coverage for https://github.com/Tinder/bazel-diff/issues/83
  // ------------------------------------------------------------------------
  // The original 2021 report (#83) was that a comment-only edit to `WORKSPACE` in a
  // rules_go + Gazelle setup caused every external Go dep -- and the `go_binary`
  // depending on them -- to be reported as impacted. The user pinned it down to a
  // single comment added to `WORKSPACE`.
  //
  // The bzlmod-era analog of that bug would be a comment-only edit to `MODULE.bazel`
  // causing the entire dep graph to re-hash. This is exactly the over-triggering shape
  // PR #314 was designed to prevent: rather than seed every target's hash with the raw
  // MODULE.bazel content, bazel-diff parses `bazel mod graph --output=json` and only
  // re-hashes targets when the resolved module graph differs.
  //
  // I verified by hand with the locally built CLI: adding a leading `# comment` line to
  // MODULE.bazel produces an empty impacted-targets list (correct).
  //
  // This passing regression test locks in that behaviour. If a future change goes back
  // to seeding hashes with raw MODULE.bazel content, this test will fail. WORKSPACE mode
  // is no longer supported in current Bazel, so a WORKSPACE-comment reproducer would need
  // a legacy-Bazel fixture; the bzlmod equivalent is the right shape going forward.
  @Test
  fun testModuleBazelCommentOnlyChangeDoesNotImpactTargets_regressionForIssue83() {
    val version = getBazelVersion()
    org.junit.Assume.assumeNotNull(version)
    val v = version!!
    val comparator =
        compareBy<Triple<Int, Int, Int>> { it.first }.thenBy { it.second }.thenBy { it.third }
    val hasModShowRepo = comparator.compare(v, Triple(8, 6, 0)) >= 0 && v != Triple(9, 0, 0)
    org.junit.Assume.assumeTrue(
        "Requires Bazel 8.6.0+ or 9.0.1+ (current: ${v.first}.${v.second}.${v.third})",
        hasModShowRepo)

    val workspaceA = copyTestWorkspace("module_bazel_comment")
    val workspaceB = copyTestWorkspace("module_bazel_comment")

    // Prepend a comment to MODULE.bazel in B. The resolved module graph is byte-identical
    // to A's; only the raw source bytes differ.
    val moduleBazelInB = File(workspaceB, "MODULE.bazel")
    val originalModule = moduleBazelInB.readText()
    moduleBazelInB.writeText("# A comment that should be a no-op for impacted targets.\n" + originalModule)

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())

    assertThat(
            cli.execute(
                "generate-hashes",
                "-w", workspaceA.absolutePath,
                "-b", "bazel",
                from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w", workspaceB.absolutePath,
                "-b", "bazel",
                to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w", workspaceB.absolutePath,
                "-b", "bazel",
                "-sh", from.absolutePath,
                "-fh", to.absolutePath,
                "-o", impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
    // Desired and current behaviour: a comment-only MODULE.bazel edit does not invalidate
    // any targets. If a future change reintroduces over-triggering, this assertion fails
    // and points back to the relevant module-graph-diffing logic.
    assertThat(impacted.isEmpty())
        .transform("A comment-only MODULE.bazel change must not impact any targets (got: $impacted)") { it }
        .isEqualTo(true)
  }

  // ------------------------------------------------------------------------
  // Regression coverage for https://github.com/Tinder/bazel-diff/issues/196
  // ------------------------------------------------------------------------
  // The original 2023 report was that bazel-diff failed to query a bzlmod workspace whose
  // dependencies were all wired through local repositories (no BCR fetches). honnix narrowed
  // it down to cquery returning labels with apparent vs canonical repo names that did not
  // match the compatible target set; PR #224 fixed that handling.
  //
  // This is a passing regression-protection test (NOT @Ignore). It exercises a minimal
  // bzlmod workspace where the only dependency uses `local_path_override`, runs
  // generate-hashes in both query and cquery modes, and asserts both produce a non-empty
  // hash JSON that covers the local-repo target. If a future change reintroduces the
  // canonical-name mismatch or breaks local_path_override resolution, this test fails.
  @Test
  fun testBzlmodLocalPathOverrideWorks_regressionForIssue196() {
    val workspace = copyTestWorkspace("bzlmod_local_repo")
    val outputDir = temp.newFolder()
    val queryOutput = File(outputDir, "query.json")
    val cqueryOutput = File(outputDir, "cquery.json")

    val cli = CommandLine(BazelDiff())

    // Query mode: must succeed and include //:consume + the synthetic //external entry.
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w", workspace.absolutePath,
                "-b", "bazel",
                queryOutput.absolutePath))
        .isEqualTo(0)
    val queryJson = queryOutput.readText()
    assertThat(queryJson.contains("//:consume"))
        .transform("query-mode hashes should include //:consume; got: $queryJson") { it }
        .isEqualTo(true)

    // cquery mode: must succeed and include the canonical @@dep_repo+ label for the
    // local-overridden repo (the exact shape that PR #224 made work).
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w", workspace.absolutePath,
                "-b", "bazel",
                "--useCquery",
                cqueryOutput.absolutePath))
        .isEqualTo(0)
    val cqueryJson = cqueryOutput.readText()
    assertThat(cqueryJson.contains("@@//:consume") || cqueryJson.contains("//:consume"))
        .transform("cquery-mode hashes should include the consumer target; got: $cqueryJson") { it }
        .isEqualTo(true)
    assertThat(cqueryJson.contains("dep_repo"))
        .transform("cquery-mode hashes should reference dep_repo (local_path_override target); got: $cqueryJson") { it }
        .isEqualTo(true)
  }

  // ------------------------------------------------------------------------
  // Regression coverage for https://github.com/Tinder/bazel-diff/issues/266
  // ------------------------------------------------------------------------
  // The user reported that updating go.mod (e.g. bumping a dependency version) did not
  // change the hash of go_test/go_library targets that depend on that module. The setup
  // is bzlmod + rules_go + gazelle's go_deps extension reading go.mod.
  //
  // I built a minimal `go_mod_change` workspace with exactly that wiring (rules_go 0.60.0,
  // gazelle 0.45.0, a `go_library` and `go_test` depending on @com_github_pkg_errors) and
  // verified by hand with the locally built CLI that current bazel-diff DOES detect the
  // change end-to-end:
  //
  //   v0.9.1 -> v0.9.0 of github.com/pkg/errors:
  //     impacted: //:lib, //:lib_test
  //
  // The mechanism is the bzlmod `mod show_repo` integration: gazelle's go_deps extension
  // re-resolves on every `bazel mod` invocation and the synthetic `//external:*` target
  // for the changed module reflects the new version, which propagates into `//:lib`'s
  // transitive hash.
  //
  // This regression-protection test (NOT @Ignore'd) locks in that behaviour. If a future
  // change to bzlmod mod show_repo handling or to gazelle's extension wiring breaks
  // go.mod tracking again, the test will fail.
  //
  // Requires Bazel 8.6.0+ for the `mod show_repo --output=streamed_proto` path. The test
  // skips itself on older Bazel via `Assume.assumeTrue` (matching the convention in
  // `testBzlmodShowRepoDetectsModuleBazelChanges`).
  @Test
  fun testGoModUpdateImpactsGoTargets_regressionForIssue266() {
    val version = getBazelVersion()
    org.junit.Assume.assumeNotNull(version)
    val v = version!!
    val comparator =
        compareBy<Triple<Int, Int, Int>> { it.first }.thenBy { it.second }.thenBy { it.third }
    val hasModShowRepo = comparator.compare(v, Triple(8, 6, 0)) >= 0 && v != Triple(9, 0, 0)
    org.junit.Assume.assumeTrue(
        "Requires Bazel 8.6.0+ or 9.0.1+ (current: ${v.first}.${v.second}.${v.third})",
        hasModShowRepo)

    val workspaceA = copyTestWorkspace("go_mod_change")
    val workspaceB = copyTestWorkspace("go_mod_change")

    // Mutate go.mod in B to depend on v0.9.0 instead of v0.9.1. go.sum in the fixture
    // already contains both versions' entries.
    val goModInB = File(workspaceB, "go.mod")
    val original = goModInB.readText()
    val mutated = original.replace("github.com/pkg/errors v0.9.1", "github.com/pkg/errors v0.9.0")
    assertThat(mutated != original).isEqualTo(true)
    goModInB.writeText(mutated)

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())

    assertThat(
            cli.execute(
                "generate-hashes",
                "-w", workspaceA.absolutePath,
                "-b", "bazel",
                from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w", workspaceB.absolutePath,
                "-b", "bazel",
                to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w", workspaceB.absolutePath,
                "-b", "bazel",
                "-sh", from.absolutePath,
                "-fh", to.absolutePath,
                "-o", impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
    val libImpacted = impacted.any { it == "//:lib" || it == "@@//:lib" }
    val libTestImpacted = impacted.any { it == "//:lib_test" || it == "@@//:lib_test" }
    assertThat(libImpacted)
        .transform("//:lib should be impacted when go.mod changes pkg/errors version; got: $impacted") { it }
        .isEqualTo(true)
    assertThat(libTestImpacted)
        .transform("//:lib_test should be impacted when go.mod changes pkg/errors version; got: $impacted") { it }
        .isEqualTo(true)
  }

  // ------------------------------------------------------------------------
  // Regression coverage for https://github.com/Tinder/bazel-diff/issues/259 and #227.
  // ------------------------------------------------------------------------
  // Both issues describe the same underlying gap: when a BUILD file `load()`s a .bzl
  // macro, editing the .bzl macro body in a way that does not change the generated rule's
  // attrs is not reflected in `bazel-diff get-impacted-targets`. The user in #259
  // noticed this regression after upgrading to Bazel 7 -- Bazel pre-7 populated
  // `Rule.skylark_environment_hash_code` in the query proto so .bzl-content changes
  // bubbled in naturally; Bazel 7+ leaves that field empty, so bazel-diff missed the change.
  //
  // The fix in BuildGraphHasher.hashAllBazelTargetsAndSourcefiles walks every BUILD
  // source file's `subincludeList` (the `subinclude` proto field, which lists every .bzl
  // loaded by that BUILD), softDigests each main-repo .bzl file, and mixes the union of
  // digests into the seed hash. This restores the pre-Bazel-7 behaviour: a `.bzl`-only
  // edit invalidates the targets that depend on it.
  //
  // The reproducer workspace `macro_invalidation` has:
  //   - `miniature.bzl` defines a `miniature(name, src)` macro that wraps `native.genrule`.
  //   - `BUILD` does `load(":miniature.bzl", "miniature")` and calls `miniature(...)` to
  //     produce `//:logo_miniature`.
  //
  // The test mutates `miniature.bzl` to add a `print()` call inside the macro body -- this
  // does not change any attribute of the emitted `native.genrule`, so a fix that only looks
  // at rule attrs would miss it. The user's example in #259 was exactly this pattern.
  @Test
  fun testMacroBzlChangeImpactsCallers_regressionForIssue259And227() {
    val workspaceA = copyTestWorkspace("macro_invalidation")
    val workspaceB = copyTestWorkspace("macro_invalidation")

    // Mutate only the macro body in B by adding a `print()` call. This deliberately does
    // not touch the genrule attrs the macro emits, so the bug shows up only via the .bzl
    // file content rather than any rule-attribute hash diff.
    val bzlInB = File(workspaceB, "miniature.bzl")
    val originalBzl = bzlInB.readText()
    val mutatedBzl = originalBzl.replace(
        "def miniature(name, src, **kwargs):",
        "def miniature(name, src, **kwargs):\n    print(\"miniature: \" + name)")
    assertThat(mutatedBzl != originalBzl).isEqualTo(true)
    bzlInB.writeText(mutatedBzl)

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w", workspaceA.absolutePath,
                "-b", "bazel",
                from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w", workspaceB.absolutePath,
                "-b", "bazel",
                to.absolutePath))
        .isEqualTo(0)

    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w", workspaceB.absolutePath,
                "-b", "bazel",
                "-sh", from.absolutePath,
                "-fh", to.absolutePath,
                "-o", impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
    val callerImpacted = impacted.any { it.contains("logo_miniature") }
    assertThat(callerImpacted)
        .transform("//:logo_miniature should be impacted by miniature.bzl edit; got: $impacted") { it }
        .isEqualTo(true)
  }

  // ------------------------------------------------------------------------
  // Regression coverage for https://github.com/Tinder/bazel-diff/issues/228
  // ------------------------------------------------------------------------
  // The user reported:
  //   "Trying to understand if bazel-diff still works for go deps brought in via the new
  //    gazelle/rules_go bzlmod mechanism ... Our hashes file does not include any of the
  //    external go dependencies since migrating to the new mechanism."
  //
  // The concern is whether the hashes JSON produced by `generate-hashes` contains an entry
  // for the external Go repository that gazelle's `go_deps` extension materializes
  // (e.g. `com_github_pkg_errors`), or whether those targets are silently dropped after a
  // bzlmod migration.
  //
  // The existing `testGoModUpdateImpactsGoTargets_regressionForIssue266` test verifies that
  // bumping a version in go.mod *propagates* to dependent main-repo targets. That is a
  // separate signal from "is the external Go module itself present in the hashes JSON".
  //
  // On a current build (Bazel 9.1) the hashes JSON for the `go_mod_change` fixture includes:
  //
  //   //:BUILD
  //   //:lib
  //   //:lib.go
  //   //:lib_test
  //   //:lib_test.go
  //   //external:com_github_pkg_errors
  //   //external:gazelle
  //   //external:rules_go
  //
  // The `//external:<apparent_name>` entries are the synthetic labels BazelClient produces
  // by querying `bazel mod show_repo --output=streamed_proto` (the same mechanism used by
  // the #255 fix). They are exactly the surface the user in #228 was looking for: a
  // per-bzlmod-module entry in the hashes JSON whose hash changes when that module's
  // resolved version changes.
  //
  // This regression-protection test (NOT @Ignore'd) locks in that behaviour. If a future
  // change drops external bzlmod entries from generate-hashes again, the test fails with a
  // diagnostic that names the missing label.
  //
  // Requires Bazel 8.6.0+ for the bzlmod / `mod show_repo --output=streamed_proto` path,
  // matching the convention used by `testGoModUpdateImpactsGoTargets_regressionForIssue266`.
  @Test
  fun testExternalGoDepsAppearInHashes_regressionForIssue228() {
    val version = getBazelVersion()
    org.junit.Assume.assumeNotNull(version)
    val v = version!!
    val comparator =
        compareBy<Triple<Int, Int, Int>> { it.first }.thenBy { it.second }.thenBy { it.third }
    val hasModShowRepo = comparator.compare(v, Triple(8, 6, 0)) >= 0 && v != Triple(9, 0, 0)
    org.junit.Assume.assumeTrue(
        "Requires Bazel 8.6.0+ or 9.0.1+ (current: ${v.first}.${v.second}.${v.third})",
        hasModShowRepo)

    val workspace = copyTestWorkspace("go_mod_change")
    val outputDir = temp.newFolder()
    val hashesPath = File(outputDir, "hashes.json")

    val cli = CommandLine(BazelDiff())
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w", workspace.absolutePath,
                "-b", "bazel",
                hashesPath.absolutePath))
        .isEqualTo(0)

    val rawJson = hashesPath.readText()
    assertThat(rawJson.isNotEmpty())
        .transform("generate-hashes produced an empty hashes file") { it }
        .isEqualTo(true)

    // Parse the hashes JSON. Format is either {"hashes": {...}, "metadata": {...}} (bzlmod /
    // new format) or just {...} (legacy). The label keys live in the inner "hashes" object
    // when present.
    val parsed = Gson().fromJson(rawJson, com.google.gson.JsonObject::class.java)
    val hashesObj = if (parsed.has("hashes")) parsed.getAsJsonObject("hashes") else parsed
    val labels = hashesObj.keySet()

    // Sanity: main-repo targets must be present.
    val libPresent = labels.any { it == "//:lib" || it == "@@//:lib" }
    assertThat(libPresent)
        .transform("hashes should include //:lib; got keys: ${labels.sorted()}") { it }
        .isEqualTo(true)

    // The #228 assertion: the external Go module brought in by gazelle's `go_deps`
    // extension must appear as a synthetic //external:<apparent_name> entry in the hashes
    // JSON. The hash of that entry is what allows a `go.mod` version bump to propagate to
    // dependent targets (issue #266) and is the per-module signal the user asked about.
    val externalGoDepLabel = labels.firstOrNull { it.contains("com_github_pkg_errors") }
    assertThat(externalGoDepLabel)
        .transform(
            "expected a hashes entry referencing the external Go dep `com_github_pkg_errors` " +
                "(per #228); got keys: ${labels.sorted()}") { it }
        .isEqualTo("//external:com_github_pkg_errors")
  }

  private fun copyTestWorkspace(path: String): File {
    val testProject = temp.newFolder()

    // Copy all of the files in path into a new folder
    val filepath = File("cli/src/test/resources/workspaces", path)
    filepath.walkTopDown().forEach { file ->
      val destFile = File(testProject, file.relativeTo(filepath).path)
      if (file.isDirectory) {
        destFile.mkdirs()
      } else {
        file.copyTo(destFile)
      }
    }
    return testProject
  }

  private fun extractFixtureProject(path: String): File {
    val testProject = temp.newFolder()
    val fixtureCopy = temp.newFile()

    fixtureCopy.outputStream().use { javaClass.getResourceAsStream(path).copyTo(it) }
    val zipFile = ZipFile(fixtureCopy)
    zipFile.stream().use {
      it.forEach { entry ->
        when {
          entry.isDirectory -> {
            Paths.get(testProject.absolutePath, entry.name).toFile().mkdirs()
          }
          else -> {
            File(testProject, entry.name)
                .apply {
                  parentFile.mkdir()
                  createNewFile()
                }
                .outputStream()
                .use { outputStream -> zipFile.getInputStream(entry).copyTo(outputStream) }
          }
        }
      }
    }

    return testProject
  }
}
