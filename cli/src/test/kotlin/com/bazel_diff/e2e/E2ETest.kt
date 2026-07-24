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
    return targets
        .filter { target ->
          // Filter out bazel-diff's own internal test targets
          !target.contains("bazel-diff-integration-test") &&
              !target.contains("@@//:BUILD") &&
              !target.contains("bazel_diff_maven") && // Filter out bazel-diff's maven dependencies
              // Filter out platform-specific Maven alias targets that may or may not appear in
              // cquery
              // results depending on Bazel version and platform (macOS vs Linux)
              !target.matches(
                  Regex(
                      ".*rules_jvm_external\\+\\+maven\\+maven//:com_google_code_findbugs_jsr305$")) &&
              !target.matches(
                  Regex(
                      ".*rules_jvm_external\\+\\+maven\\+maven//:com_google_guava_failureaccess$")) &&
              !target.matches(
                  Regex(
                      ".*rules_jvm_external\\+\\+maven\\+maven//:com_google_guava_listenablefuture$")) &&
              !target.matches(
                  Regex(
                      ".*rules_jvm_external\\+//private/tools/java/com/github/bazelbuild/rules_jvm_external/jar:AddJarManifestEntry$")) &&
              // Filter out junit and hamcrest which may appear on some platforms
              !target.matches(Regex(".*rules_jvm_external\\+\\+maven\\+maven//:junit_junit$")) &&
              !target.matches(
                  Regex(".*rules_jvm_external\\+\\+maven\\+maven//:org_hamcrest_hamcrest_core$"))
        }
        .toSet()
  }

  private fun assertTargetsMatch(
      actual: Set<String>,
      expected: Set<String>,
      testContext: String = ""
  ) {
    if (actual != expected) {
      val missingTargets = expected - actual
      val unexpectedTargets = actual - expected

      val debugMessage = buildString {
        appendLine("\n========================================")
        appendLine(
            "Target list mismatch${if (testContext.isNotEmpty()) " in $testContext" else ""}")
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

    val actual: Set<String> =
        filterBazelDiffInternalTargets(
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

  /**
   * End-to-end coverage for the `serve` query service: builds a two-commit git repo from the
   * shell-only `distance_metrics` workspace, runs `bazel-diff serve` (with `--trackDeps`) in a
   * background thread, then hits `/health`, `/impacted_targets`, and
   * `/impacted_targets_with_distances` over real HTTP. Exercises the full
   * [com.bazel_diff.cli.ServeCommand] path (Koin + hasher wiring, git checkout, real `bazel query`
   * for both revisions, dep-edge tracking, distance computation, and the cache) the same way the
   * other E2E tests cover the CLI commands.
   */
  @Test
  fun testServeEndToEnd() {
    val workspace = copyTestWorkspace("distance_metrics")
    fun git(vararg args: String): String {
      val proc =
          ProcessBuilder(listOf("git") + args)
              .directory(workspace)
              .redirectErrorStream(true)
              .start()
      val output = proc.inputStream.readBytes().decodeToString()
      check(proc.waitFor() == 0) { "git ${args.joinToString(" ")} failed: $output" }
      return output.trim()
    }
    git("init", "-q")
    git("config", "user.email", "test@example.com")
    git("config", "user.name", "test")
    git("add", "-A")
    git("commit", "-q", "-m", "base")
    val fromSha = git("rev-parse", "HEAD")
    File(workspace, "lib.sh").writeText("echo changed\n")
    git("add", "-A")
    git("commit", "-q", "-m", "change lib.sh")
    val toSha = git("rev-parse", "HEAD")

    val cacheDir = temp.newFolder()
    val port = java.net.ServerSocket(0).use { it.localPort }

    val serveThread =
        Thread {
              CommandLine(BazelDiff())
                  .execute(
                      "serve",
                      "-w",
                      workspace.absolutePath,
                      "-b",
                      "bazel",
                      "--cacheDir",
                      cacheDir.absolutePath,
                      "--port",
                      port.toString(),
                      "--no-initial-fetch",
                      "--trackDeps")
            }
            .apply {
              isDaemon = true
              start()
            }

    try {
      awaitServeHealthy(port)
      val (code, body) =
          httpGetServe("http://localhost:$port/impacted_targets?from=$fromSha&to=$toSha")
      assertThat(code).isEqualTo(200)

      val parsed: Map<String, Any> =
          Gson().fromJson(body, object : TypeToken<Map<String, Any>>() {}.type)
      assertThat(parsed["from"]).isEqualTo(fromSha)
      assertThat(parsed["to"]).isEqualTo(toSha)
      @Suppress("UNCHECKED_CAST") val impacted = parsed["impactedTargets"] as List<String>
      // Editing lib.sh must impact at least its own sh_library target.
      assertThat(impacted.contains("//:lib")).isEqualTo(true)

      // The distances endpoint returns the same impacted targets, each annotated with build-graph
      // distance metrics. //:lib is directly edited, so it sits at distance 0.
      val (distCode, distBody) =
          httpGetServe(
              "http://localhost:$port/impacted_targets_with_distances?from=$fromSha&to=$toSha")
      assertThat(distCode).isEqualTo(200)
      val distParsed: Map<String, Any> =
          Gson().fromJson(distBody, object : TypeToken<Map<String, Any>>() {}.type)
      assertThat(distParsed["from"]).isEqualTo(fromSha)
      assertThat(distParsed["to"]).isEqualTo(toSha)
      @Suppress("UNCHECKED_CAST")
      val withDistances = distParsed["impactedTargets"] as List<Map<String, Any>>
      val libEntry = withDistances.single { it["label"] == "//:lib" }
      // Gson decodes JSON numbers as Double.
      assertThat(libEntry["targetDistance"]).isEqualTo(0.0)
      assertThat(libEntry["packageDistance"]).isEqualTo(0.0)
    } finally {
      serveThread.interrupt()
      serveThread.join(10_000)
    }
  }

  /** Polls `/health` until it returns 200, up to ~30s, failing the test otherwise. */
  private fun awaitServeHealthy(port: Int) {
    repeat(60) {
      try {
        if (httpGetServe("http://localhost:$port/health").first == 200) return
      } catch (_: Exception) {
        // server not up yet
      }
      Thread.sleep(500)
    }
    throw AssertionError("serve /health never became ready on port $port")
  }

  private fun httpGetServe(url: String): Pair<Int, String> {
    val conn = java.net.URL(url).openConnection() as java.net.HttpURLConnection
    conn.requestMethod = "GET"
    conn.connectTimeout = 5_000
    // Generous read timeout: a cold /impacted_targets runs `bazel query` twice.
    conn.readTimeout = 300_000
    return try {
      val code = conn.responseCode
      val stream = if (code in 200..299) conn.inputStream else conn.errorStream
      val text = stream?.bufferedReader()?.use { it.readText() } ?: ""
      code to text
    } finally {
      conn.disconnect()
    }
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
    val exitCode =
        cli.execute(
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
    testE2E(listOf("--no-keep_going"), emptyList(), "/fixture/impacted_targets-1-2.txt")
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
    val exitCode =
        cli.execute(
            "generate-hashes",
            "-w",
            workspace.absolutePath,
            "-b",
            "bazel",
            "--useCquery",
            output.absolutePath)

    assertThat(exitCode).isEqualTo(0)

    val hashes: Map<String, Any> =
        Gson().fromJson(output.readText(), object : TypeToken<Map<String, Any>>() {}.type)
    assertThat(hashes.isNotEmpty()).isEqualTo(true)
  }

  @Test
  fun testExcludeTargetsQueryFiltersManualTargets() {
    // Reproducer + fix for https://github.com/Tinder/bazel-diff/issues/392: --excludeTargetsQuery
    // lets users drop `manual`-tagged targets (or any query-matched targets) from generate-hashes
    // output via Bazel's `except` operator. Reuses the locked `distance_metrics` workspace and
    // appends a manual-tagged target to the *copied* BUILD so no new MODULE.bazel.lock is needed.
    val workspace = copyTestWorkspace("distance_metrics")
    File(workspace, "BUILD")
        .appendText(
            "\nsh_library(\n" +
                "    name = \"manual_only\",\n" +
                "    srcs = [\"lib.sh\"],\n" +
                "    tags = [\"manual\"],\n" +
                ")\n")

    val outputDir = temp.newFolder()
    val withoutFilter = File(outputDir, "without_filter.json")
    val withFilter = File(outputDir, "with_filter.json")
    val cli = CommandLine(BazelDiff())

    // Baseline: the manual target is present when no filter is applied.
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w",
                workspace.absolutePath,
                "-b",
                "bazel",
                withoutFilter.absolutePath))
        .isEqualTo(0)
    val baseline = readTargetHashes(withoutFilter)
    assertThat(baseline.containsKey("//:manual_only")).isEqualTo(true)
    assertThat(baseline.containsKey("//:lib")).isEqualTo(true)

    // With the filter, the manual target is gone but the normal target remains. The regex is
    // Bazel's documented incantation for matching the `manual` element of a `tags` list.
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w",
                workspace.absolutePath,
                "-b",
                "bazel",
                "--excludeTargetsQuery",
                """attr("tags", "[\[ ]manual[,\]]", //...)""",
                withFilter.absolutePath))
        .isEqualTo(0)
    val filtered = readTargetHashes(withFilter)
    assertThat(filtered.containsKey("//:manual_only")).isEqualTo(false)
    assertThat(filtered.containsKey("//:lib")).isEqualTo(true)
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

    val actual: Set<String> =
        filterBazelDiffInternalTargets(
            impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    val expected: Set<String> =
        javaClass
            .getResourceAsStream(
                "/fixture/fine-grained-hash-external-repo-test-impacted-targets.txt")
            .use {
              filterBazelDiffInternalTargets(
                  it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet())
            }

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

    val actual: Set<String> =
        filterBazelDiffInternalTargets(
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

    val actual: Set<String> =
        filterBazelDiffInternalTargets(
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
        emptyList(), "@bazel_diff_maven", "/fixture/bzlmod-transitive-test-impacted-targets.txt")
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

    val actual: Set<String> =
        filterBazelDiffInternalTargets(
            impactedTargetsOutput
                .readLines()
                .filter { it.isNotBlank() && !it.startsWith("#") }
                .toSet())
    val expected: Set<String> =
        javaClass.getResourceAsStream(expectedResultFile).use {
          filterBazelDiffInternalTargets(
              it.bufferedReader()
                  .readLines()
                  .filter { it.isNotBlank() && !it.startsWith("#") }
                  .toSet())
        }

    assertTargetsMatch(actual, expected, "testBzlmodCCTransitiveDeps")
  }

  @Test
  @org.junit.Ignore(
      "Skipped due to Bazel version compatibility issues in test environment. " +
          "The fixtures use Bazel 7.0.0 but test environment uses Bazel 8+, causing bzlmod/abseil-cpp " +
          "package loading errors. This test is ready to run when Bazel version handling is resolved. " +
          "Module graph hashing has been implemented and should detect transitive dependency changes.")
  fun testBzlmodCCTransitiveDepsQuery() {
    // This test validates that MODULE.bazel changes are now detected via module graph hashing.
    // Both target-a and target-b should be impacted when abseil-cpp version changes.
    testBzlmodCCTransitiveDeps(
        emptyList(), "/fixture/bzlmod-cc-transitive-test-impacted-targets.txt")
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

    var actual: Set<String> =
        filterBazelDiffInternalTargets(
            impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    var expected: Set<String> =
        javaClass
            .getResourceAsStream("/fixture/cquery-test-guava-upgrade-android-impacted-targets.txt")
            .use {
              filterBazelDiffInternalTargets(
                  it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet())
            }

    assertTargetsMatch(
        actual, expected, "testUseCqueryWithExternalDependencyChange - Android platform")

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

    actual =
        filterBazelDiffInternalTargets(
            impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    expected =
        javaClass
            .getResourceAsStream("/fixture/cquery-test-guava-upgrade-jre-impacted-targets.txt")
            .use {
              filterBazelDiffInternalTargets(
                  it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet())
            }

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

    var actual: Set<String> =
        filterBazelDiffInternalTargets(
            impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    var expected: Set<String> =
        javaClass
            .getResourceAsStream(
                "/fixture/cquery-test-android-code-change-android-impacted-targets.txt")
            .use {
              filterBazelDiffInternalTargets(
                  it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet())
            }

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

    actual =
        filterBazelDiffInternalTargets(
            impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())
    expected =
        javaClass
            .getResourceAsStream(
                "/fixture/cquery-test-android-code-change-jre-impacted-targets.txt")
            .use {
              filterBazelDiffInternalTargets(
                  it.bufferedReader().readLines().filter { it.isNotBlank() }.toSet())
            }

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
        gson
            .fromJson<List<Map<String, Any>>>(impactedTargetsOutput.readText(), shape)
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
    val exitCodeWithoutCquery =
        cli.execute(
            "generate-hashes", "-w", workspace.absolutePath, "-b", "bazel", from.absolutePath)

    assertThat(exitCodeWithoutCquery).isEqualTo(0)

    // Now, verify that generate-hashes fails with --useCquery due to the failing target
    // This demonstrates the issue described in #301
    val exitCodeWithCquery =
        cli.execute(
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
    val exitCodeWithNoKeepGoing =
        cli.execute(
            "generate-hashes",
            "-w",
            workspace.absolutePath,
            "-b",
            "bazel",
            "--useCquery",
            "--no-keep_going",
            outputNoKeepGoing.absolutePath)
    assertThat(exitCodeWithNoKeepGoing).isEqualTo(1)

    // Test with --keep_going explicitly enabled (no longer the default)
    // With keep_going, cquery returns partial results but still exits with code 1
    // The current implementation allows exit codes 0 and 3, but cquery with keep_going
    // returns exit code 1 when some targets fail analysis
    val exitCodeWithCqueryKeepGoing =
        cli.execute(
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
    val exitCodeWithCustomExpression =
        cli.execute(
            "generate-hashes",
            "-w",
            workspace.absolutePath,
            "-b",
            "bazel",
            "--useCquery",
            "--cqueryExpression",
            "deps(//:normal_target) + deps(//:dependent_target)",
            from.absolutePath)

    // With the custom expression that explicitly lists only the non-failing targets, this should
    // succeed
    assertThat(exitCodeWithCustomExpression).isEqualTo(0)

    // Verify the hashes were generated successfully and contain the expected targets
    val hashes = from.readText()
    assertThat(hashes.contains("normal_target")).isEqualTo(true)
    assertThat(hashes.contains("dependent_target")).isEqualTo(true)
    // The failing target should not be in the hashes since it wasn't included in the query
    assertThat(hashes.contains("failing_analysis_target")).isEqualTo(false)
  }

  @Test
  fun testKeepGoingSilentlyDropsTargetsOnRepoRuleFailure_reproducerForIssue398() {
    // Reproducer for https://github.com/Tinder/bazel-diff/issues/398
    //
    // When `--keep_going` is enabled, bazel-diff treats a partial
    // `bazel query` (exit code 3) as success. When a repository rule fails to resolve --
    // e.g. a transient network error fetching a remote dependency such as
    //
    //   fetch_repo: buf.build/go/protovalidate@v1.1.3:
    //       Get "https://proxy.golang.org/...": net/http: TLS handshake timeout
    //
    // -- the package that references that repo silently disappears from the query results,
    // and bazel-diff emits a hash set that is missing those targets WITHOUT any error. This
    // makes hashes non-deterministic across runs (a target present when the fetch succeeds
    // vanishes when it flakes). `--keep_going` now defaults to `false`, so the default behavior
    // fails loudly and keeps hashes deterministic.
    //
    // The `keep_going_repo_failure` workspace has two packages:
    //   //good -- a plain genrule with no external deps (always resolvable)
    //   //bad  -- loads a .bzl from @failing_dep, whose repository rule always fails to fetch
    //
    // This test locks in the default (`--no-keep_going`) fail-loud behavior and documents the
    // opt-in `--keep_going` behavior that silently drops targets.
    val workspace = copyTestWorkspace("keep_going_repo_failure")
    val outputDir = temp.newFolder()

    val cli = CommandLine(BazelDiff())

    // Default behavior (--keep_going=false): bazel query fails outright (non-zero, non-partial
    // exit code) and bazel-diff surfaces the error instead of writing a truncated hash set.
    val defaultOutput = File(outputDir, "hashes_default.json")
    val defaultExit =
        cli.execute(
            "generate-hashes",
            "-w",
            workspace.absolutePath,
            "-b",
            "bazel",
            defaultOutput.absolutePath)
    assertThat(defaultExit).isEqualTo(1)

    // Opt-in --keep_going: generate-hashes SUCCEEDS despite the unresolvable repo, because
    // bazel query returns exit code 3 (partial results) which bazel-diff allows. The healthy
    // //good target is hashed, but //bad has been silently dropped -- this is the
    // incorrect/non-deterministic hash set that motivated flipping the default.
    val keepGoingOutput = File(outputDir, "hashes_keep_going.json")
    val keepGoingExit =
        cli.execute(
            "generate-hashes",
            "-w",
            workspace.absolutePath,
            "-b",
            "bazel",
            "--keep_going",
            keepGoingOutput.absolutePath)
    assertThat(keepGoingExit).isEqualTo(0)

    val keepGoingHashes = keepGoingOutput.readText()
    assertThat(keepGoingHashes.contains("//good:good")).isEqualTo(true)
    assertThat(keepGoingHashes.contains("//bad:")).isEqualTo(false)
  }

  @Test
  fun testUndeclaredWorkspaceReadIsNotImpacted_reproducerForIssue401() {
    // Reproducer for https://github.com/Tinder/bazel-diff/issues/401
    //
    // Feature request: "always-affected" hashing for targets that perform undeclared
    // workspace reads (non-hermetic targets). bazel-diff derives a target's hash purely
    // from its DECLARED graph -- srcs, deps, and attributes. A target that reads files it
    // does not declare (e.g. a `buildifier_test` that scans every `.bzl` in the repo
    // without listing them in `srcs`) therefore gets a STABLE hash even when one of those
    // undeclared files changes, so `get-impacted-targets` skips it -- even though the test
    // would fail if actually run.
    //
    // The `always_affected_external` workspace has two genrules:
    //   //:scanner  -- tagged `external`, its action reads `scanned_data.txt` but does NOT
    //                  declare it as a src (the undeclared / non-hermetic read).
    //   //:hermetic -- a normal target that declares `declared_src.txt` as a src.
    //
    // Between the two checkouts we edit BOTH files. The hermetic target is correctly
    // reported as impacted; the `external`-tagged scanner is NOT -- pinning the current
    // behaviour that motivates the feature request. If an `--alwaysAffectedTags`
    // (or similar) feature lands, the scanner should start appearing in the impacted set
    // and this assertion will flag that the behaviour changed.
    val workspaceA = copyTestWorkspace("always_affected_external")
    val workspaceB = copyTestWorkspace("always_affected_external")

    // Change the UNDECLARED read consumed by //:scanner and the DECLARED src of //:hermetic.
    File(workspaceB, "scanned_data.txt").writeText("v2\n")
    File(workspaceB, "declared_src.txt").writeText("goodbye\n")

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())

    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceA.absolutePath, "-b", "bazel", from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceB.absolutePath, "-b", "bazel", to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()

    // The hermetic target declared its changed source, so it is correctly impacted.
    assertThat(impacted.contains("//:hermetic")).isEqualTo(true)

    // The `external`-tagged scanner read a file it never declared. Its hash is unchanged,
    // so bazel-diff does NOT report it -- this is exactly the gap #401 asks to close.
    assertThat(impacted.contains("//:scanner")).isEqualTo(false)
  }

  @Test
  fun testPackageGroupChangeImpactsConsumers_regressionForIssue441() {
    // Regression test for the under-invalidation (false-negative) half of
    // https://github.com/Tinder/bazel-diff/issues/441.
    //
    // bazel-diff used to keep only targets whose query `Discriminator` is RULE,
    // SOURCE_FILE, or GENERATED_FILE; `PACKAGE_GROUP` was dropped
    // (BazelQueryService.toBazelTarget logged "Unsupported target type" and
    // returned null), so a change to a package_group's `packages` list was never
    // reflected in any hash -- even though it genuinely alters downstream
    // visibility and can flip a consumer from building to failing.
    //
    // The `package_group_dropped` workspace:
    //   //lib:consumers -- a package_group (created by a macro in lib/defs.bzl)
    //                      that gates the visibility of //lib:thing. Its
    //                      allow-list lives in defs.bzl as `ALLOWED`.
    //   //lib:thing     -- a genrule declared DIRECTLY (not via the macro), so
    //                      its per-rule `.bzl` seed never picks up defs.bzl.
    //   //consumer:use_thing -- depends on //lib:thing.
    //
    // Between the two checkouts we edit ONLY lib/defs.bzl, emptying `ALLOWED`.
    // That revokes //consumer's visibility of //lib:thing: workspace A builds
    // //consumer:use_thing successfully, workspace B fails visibility analysis
    // for it. The BUILD files and every native rule are byte-for-byte identical
    // across the checkouts, so the sole semantic change rides entirely on the
    // PACKAGE_GROUP target.
    //
    // Fixed behaviour: the package_group is lowered to a synthetic rule and
    // hashed, and RuleHasher follows the (nodep) `visibility` edge from
    // //lib:thing to it -- so the group, the rule it gates, and that rule's
    // transitive dependents are all reported.
    val workspaceA = copyTestWorkspace("package_group_dropped")
    val workspaceB = copyTestWorkspace("package_group_dropped")

    // The ONLY change: revoke //consumer's visibility by emptying the macro's
    // allow-list. Nothing else -- no BUILD file, no rule attribute -- changes.
    val defsInB = File(workspaceB, "lib/defs.bzl")
    defsInB.writeText(defsInB.readText().replace("ALLOWED = [\"//consumer\"]", "ALLOWED = []"))

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())

    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceA.absolutePath, "-b", "bazel", from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceB.absolutePath, "-b", "bazel", to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted =
        filterBazelDiffInternalTargets(
            impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet())

    // The changed package_group itself, the rule it gates (via the visibility
    // edge), and that rule's transitive dependents -- including generated files
    // -- are all reported. In particular //consumer:use_thing, which really does
    // flip from building to failing visibility analysis, is now surfaced.
    assertThat(impacted)
        .isEqualTo(
            setOf(
                "//lib:consumers",
                "//lib:thing",
                "//lib:thing.txt",
                "//consumer:use_thing",
                "//consumer:use_thing.txt"))
  }

  /**
   * Returns the Bazel version triple by running `bazel version`, or null if it cannot be
   * determined.
   */
  private fun getBazelVersion(): Triple<Int, Int, Int>? {
    return try {
      val process = ProcessBuilder("bazel", "version").redirectErrorStream(true).start()
      val output = process.inputStream.bufferedReader().readText()
      process.waitFor()
      val versionLine =
          output
              .lines()
              .firstOrNull { it.startsWith("Build label: ") }
              ?.removePrefix("Build label: ")
              ?.trim() ?: return null
      val parts =
          versionLine.split('-')[0].split('.').map { it.takeWhile { c -> c.isDigit() }.toInt() }
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
    val comparator =
        compareBy<Triple<Int, Int, Int>> { it.first }.thenBy { it.second }.thenBy { it.third }
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
    val exitFrom =
        cli.execute(
            "generate-hashes", "-w", projectA.absolutePath, "-b", bazelPath, from.absolutePath)
    assertThat(exitFrom).isEqualTo(0)

    val exitTo =
        cli.execute(
            "generate-hashes", "-w", projectB.absolutePath, "-b", bazelPath, to.absolutePath)
    assertThat(exitTo).isEqualTo(0)

    // Get impacted targets
    cli.execute(
        "get-impacted-targets",
        "-w",
        projectB.absolutePath,
        "-b",
        bazelPath,
        "-sh",
        from.absolutePath,
        "-fh",
        to.absolutePath,
        "-o",
        impactedTargetsOutput.absolutePath)

    val impactedTargets = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()

    // guava-user depends on an external maven artifact (guava), so it must be impacted
    // when the guava version changes in MODULE.bazel.
    val guavaUserImpacted = impactedTargets.any { it.contains("guava-user") }
    assertThat(guavaUserImpacted)
        .transform("guava-user should be in impacted targets: $impactedTargets") { it }
        .isEqualTo(true)

    // bazel-diff-integration-lib depends only on local targets (Submodule), NOT on any
    // external maven artifact. It should NOT be impacted by the guava version change.
    val integrationLibImpacted =
        impactedTargets.any {
          it.contains("bazel-diff-integration-lib") && !it.contains("libbazel-diff-integration-lib")
        }
    assertThat(integrationLibImpacted)
        .transform(
            "bazel-diff-integration-lib should NOT be in impacted targets: $impactedTargets") {
              it
            }
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
    val comparator =
        compareBy<Triple<Int, Int, Int>> { it.first }.thenBy { it.second }.thenBy { it.third }
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
                "generate-hashes", "-w", projectA.absolutePath, "-b", bazelPath, from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes", "-w", projectB.absolutePath, "-b", bazelPath, to.absolutePath))
        .isEqualTo(0)

    // Default invocation: bzlmod is detected, so --excludeExternalTargets auto-defaults to true.
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                projectB.absolutePath,
                "-b",
                bazelPath,
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                defaultOutput.absolutePath))
        .isEqualTo(0)

    val defaultLines = defaultOutput.readLines().filter { it.isNotBlank() }
    val leakedExternal = defaultLines.filter { it.startsWith("//external:") }
    assertThat(leakedExternal.isEmpty())
        .transform(
            "default impacted-targets output should not contain //external:* labels, but found: $leakedExternal") {
              it
            }
        .isEqualTo(true)

    // Opt-out: --no-excludeExternalTargets reproduces the pre-#326 behavior so the synthetic
    // labels show up. This proves the labels really exist in the hashes (so the filter is doing
    // real work) and gives users an escape hatch.
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                projectB.absolutePath,
                "-b",
                bazelPath,
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "--no-excludeExternalTargets",
                "-o",
                optOutOutput.absolutePath))
        .isEqualTo(0)

    val optOutLines = optOutOutput.readLines().filter { it.isNotBlank() }
    val externalsWithOptOut = optOutLines.filter { it.startsWith("//external:") }
    assertThat(externalsWithOptOut.isNotEmpty())
        .transform(
            "with --no-excludeExternalTargets, the impacted-targets output should expose synthetic //external:* labels (none found in: $optOutLines)") {
              it
            }
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
    moduleBazelInB.writeText(
        "# A comment that should be a no-op for impacted targets.\n" + originalModule)

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())

    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceA.absolutePath, "-b", "bazel", from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceB.absolutePath, "-b", "bazel", to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
    // Desired and current behaviour: a comment-only MODULE.bazel edit does not invalidate
    // any targets. If a future change reintroduces over-triggering, this assertion fails
    // and points back to the relevant module-graph-diffing logic.
    assertThat(impacted.isEmpty())
        .transform(
            "A comment-only MODULE.bazel change must not impact any targets (got: $impacted)") {
              it
            }
        .isEqualTo(true)
  }

  // ------------------------------------------------------------------------
  // Hermetic, cross-machine hashing
  // ------------------------------------------------------------------------
  // bazel-diff's whole value proposition for distributed CI is that the "from" hashes can be
  // produced on one agent and the "to" hashes on another, then diffed. That is only sound if
  // generate-hashes is hermetic: byte-identical sources must hash identically regardless of the
  // absolute workspace path or the per-workspace Bazel output base (which Bazel derives from the
  // workspace path and which, on real build agents, embeds machine-specific directories like
  // `/var/lib/buildkite-agent/builds/<agent>-<instance>/...`). If any such path leaked into a
  // digest, two agents would compute different hashes for the same commit and the diff would be
  // meaningless. These tests lock that guarantee in for a bzlmod workspace.

  /**
   * Parses a generate-hashes output file and returns the inner target => hash map, transparently
   * handling both the `{"hashes": {...}, "metadata": {...}}` (bzlmod) and the legacy flat `{...}`
   * formats.
   */
  private fun parseHashesMap(file: File): Map<String, Any> {
    val parsed = Gson().fromJson(file.readText(), com.google.gson.JsonObject::class.java)
    val hashesObj = if (parsed.has("hashes")) parsed.getAsJsonObject("hashes") else parsed
    return Gson().fromJson(hashesObj, object : TypeToken<Map<String, Any>>() {}.type)
  }

  /**
   * Skips the calling test unless the local Bazel supports `mod show_repo --output=streamed_proto`
   * (8.6.0+, or 9.0.1+). Mirrors the gating used by the other bzlmod E2E tests.
   */
  private fun assumeBazelSupportsModShowRepo() {
    val version = getBazelVersion()
    org.junit.Assume.assumeNotNull(version)
    val v = version!!
    val comparator =
        compareBy<Triple<Int, Int, Int>> { it.first }.thenBy { it.second }.thenBy { it.third }
    val hasModShowRepo = comparator.compare(v, Triple(8, 6, 0)) >= 0 && v != Triple(9, 0, 0)
    org.junit.Assume.assumeTrue(
        "Requires Bazel 8.6.0+ or 9.0.1+ (current: ${v.first}.${v.second}.${v.third})",
        hasModShowRepo)
  }

  @Test
  fun testGenerateHashesIsHermeticAcrossWorkspacePaths() {
    // Two checkouts of byte-identical sources at different absolute paths -- standing in for the
    // same commit checked out on two different CI agents. Bazel gives each its own output base
    // (derived from the workspace path), so this also exercises differing output bases. The
    // resulting target hashes must be identical.
    assumeBazelSupportsModShowRepo()

    val machineX = copyTestWorkspace("module_bazel_comment")
    val machineY = copyTestWorkspace("module_bazel_comment")

    val outputDir = temp.newFolder()
    val hashesX = File(outputDir, "hashes_machine_x.json")
    val hashesY = File(outputDir, "hashes_machine_y.json")

    val cli = CommandLine(BazelDiff())
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w",
                machineX.absolutePath,
                "-b",
                "bazel",
                hashesX.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w",
                machineY.absolutePath,
                "-b",
                "bazel",
                hashesY.absolutePath))
        .isEqualTo(0)

    val hashesMapX = parseHashesMap(hashesX)
    val hashesMapY = parseHashesMap(hashesY)

    // Sanity: make sure we actually hashed the workspace's targets, so an empty-vs-empty
    // comparison can't masquerade as success.
    assertThat(hashesMapX.keys.any { it.endsWith(":lib") || it.endsWith(":bin") })
        .transform("expected workspace targets in hashes; got keys: ${hashesMapX.keys.sorted()}") {
          it
        }
        .isEqualTo(true)

    assertTargetsMatch(hashesMapX.keys, hashesMapY.keys, "hermetic hashes (key sets)")
    assertThat(hashesMapX)
        .transform("hashes computed at different workspace paths must be byte-for-byte identical") {
          it
        }
        .isEqualTo(hashesMapY)
  }

  @Test
  fun testModuleBazelLockReformatDoesNotImpactTargets() {
    // A MODULE.bazel.lock change that does not alter the resolved module graph -- e.g. the file is
    // re-serialised with different formatting, regenerated by a different Bazel patch release, or
    // simply differs between two agents -- must not invalidate any targets. bazel-diff keys bzlmod
    // change detection off the resolved module graph (`bazel mod graph`), not the raw lock-file
    // bytes, so this kind of lock churn (common across machines) stays a no-op. Companion to the
    // MODULE.bazel comment-only regression test for issue #83.
    assumeBazelSupportsModShowRepo()

    val workspaceA = copyTestWorkspace("module_bazel_comment")
    val workspaceB = copyTestWorkspace("module_bazel_comment")

    // Re-serialise MODULE.bazel.lock in B compactly: same resolved modules and registry hashes,
    // different bytes on disk. This is the lock-file analog of the #83 MODULE.bazel comment edit.
    val lockInB = File(workspaceB, "MODULE.bazel.lock")
    val originalLock = lockInB.readText()
    val reserialised =
        Gson().toJson(Gson().fromJson(originalLock, com.google.gson.JsonObject::class.java))
    assertThat(reserialised != originalLock)
        .transform("expected the re-serialised MODULE.bazel.lock to differ on disk") { it }
        .isEqualTo(true)
    lockInB.writeText(reserialised)

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceA.absolutePath, "-b", "bazel", from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceB.absolutePath, "-b", "bazel", to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
    assertThat(impacted.isEmpty())
        .transform(
            "A MODULE.bazel.lock reformat with no resolved-graph change must not impact any " +
                "targets (got: $impacted)") {
              it
            }
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
                "-w",
                workspace.absolutePath,
                "-b",
                "bazel",
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
                "-w",
                workspace.absolutePath,
                "-b",
                "bazel",
                "--useCquery",
                cqueryOutput.absolutePath))
        .isEqualTo(0)
    val cqueryJson = cqueryOutput.readText()
    assertThat(cqueryJson.contains("@@//:consume") || cqueryJson.contains("//:consume"))
        .transform("cquery-mode hashes should include the consumer target; got: $cqueryJson") { it }
        .isEqualTo(true)
    assertThat(cqueryJson.contains("dep_repo"))
        .transform(
            "cquery-mode hashes should reference dep_repo (local_path_override target); got: $cqueryJson") {
              it
            }
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
                "generate-hashes", "-w", workspaceA.absolutePath, "-b", "bazel", from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceB.absolutePath, "-b", "bazel", to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
    val libImpacted = impacted.any { it == "//:lib" || it == "@@//:lib" }
    val libTestImpacted = impacted.any { it == "//:lib_test" || it == "@@//:lib_test" }
    assertThat(libImpacted)
        .transform(
            "//:lib should be impacted when go.mod changes pkg/errors version; got: $impacted") {
              it
            }
        .isEqualTo(true)
    assertThat(libTestImpacted)
        .transform(
            "//:lib_test should be impacted when go.mod changes pkg/errors version; got: $impacted") {
              it
            }
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
    val mutatedBzl =
        originalBzl.replace(
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
                "generate-hashes", "-w", workspaceA.absolutePath, "-b", "bazel", from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceB.absolutePath, "-b", "bazel", to.absolutePath))
        .isEqualTo(0)

    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
    val callerImpacted = impacted.any { it.contains("logo_miniature") }
    assertThat(callerImpacted)
        .transform("//:logo_miniature should be impacted by miniature.bzl edit; got: $impacted") {
          it
        }
        .isEqualTo(true)
  }

  // ------------------------------------------------------------------------
  // Reproducer: a *remote* proto module version bump must invalidate the
  // main-repo targets that consume its generated Java protos.
  // ------------------------------------------------------------------------
  // Question this answers: if an external dependency that ships .proto files and
  // exposes `java_proto_library` targets gets a version bump, do the targets that
  // depend on those Java protos show up as impacted?
  //
  // The `proto_external_version_bump` workspace wires up exactly that shape:
  //   - proto_dep: a module (resolved via local_path_override so the test stays
  //     hermetic, but standing in for any remote/BCR module) that ships
  //     `greeting.proto` and exposes `@proto_dep//:greeting_java_proto`.
  //   - //:consumer: a main-repo java_library that depends on
  //     `@proto_dep//:greeting_java_proto`.
  //
  // We simulate a proto_dep 1.0.0 -> 2.0.0 release the way a real version bump
  // looks: the module `version` is bumped (root + dep MODULE.bazel) AND the
  // shipped proto gains a new field. With `--fineGrainedHashExternalRepos
  // @proto_dep` the external repo's contents are hashed, so the proto change must
  // propagate `@proto_dep//:greeting_proto` -> `@proto_dep//:greeting_java_proto`
  // -> `//:consumer`.
  //
  // Verified by hand with the locally built CLI: the bump impacts //:consumer,
  // @proto_dep//:greeting.proto, @proto_dep//:greeting_proto, and
  // @proto_dep//:greeting_java_proto (the unchanged @proto_dep//:BUILD keeps its
  // hash). This test locks that in: if fine-grained external hashing ever stops
  // descending into proto / java_proto_library targets, //:consumer would
  // silently stop being impacted and this test would fail.
  @Test
  fun testRemoteProtoVersionBumpImpactsConsumer() {
    val workspaceA = copyTestWorkspace("proto_external_version_bump")
    val workspaceB = copyTestWorkspace("proto_external_version_bump")

    // Bump proto_dep 1.0.0 -> 2.0.0 in B (root + dep MODULE.bazel)...
    val rootModuleB = File(workspaceB, "MODULE.bazel")
    val rootOriginal = rootModuleB.readText()
    val rootBumped =
        rootOriginal.replace(
            "bazel_dep(name = \"proto_dep\", version = \"1.0.0\")",
            "bazel_dep(name = \"proto_dep\", version = \"2.0.0\")")
    assertThat(rootBumped != rootOriginal).isEqualTo(true)
    rootModuleB.writeText(rootBumped)

    val depModuleB = File(workspaceB, "proto_dep/MODULE.bazel")
    depModuleB.writeText(
        depModuleB.readText().replace("version = \"1.0.0\"", "version = \"2.0.0\""))

    // ...and ship a new field in the proto, mirroring a real upstream release.
    val protoB = File(workspaceB, "proto_dep/greeting.proto")
    val protoOriginal = protoB.readText()
    val protoMutated =
        protoOriginal.replace(
            "message Greeting {\n  string message = 1;\n}",
            "message Greeting {\n  string message = 1;\n  string locale = 2;\n}")
    assertThat(protoMutated != protoOriginal).isEqualTo(true)
    protoB.writeText(protoMutated)

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w",
                workspaceA.absolutePath,
                "-b",
                "bazel",
                "--fineGrainedHashExternalRepos",
                "@proto_dep",
                from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "--fineGrainedHashExternalRepos",
                "@proto_dep",
                to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
    val consumerImpacted = impacted.any { it == "//:consumer" || it == "@@//:consumer" }
    val javaProtoImpacted = impacted.any { it.endsWith("proto_dep//:greeting_java_proto") }
    assertThat(consumerImpacted)
        .transform("//:consumer should be impacted by a proto_dep version bump; got: $impacted") {
          it
        }
        .isEqualTo(true)
    assertThat(javaProtoImpacted)
        .transform(
            "@proto_dep//:greeting_java_proto should be impacted by the proto change; got: $impacted") {
              it
            }
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
                "-w",
                workspace.absolutePath,
                "-b",
                "bazel",
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
                "(per #228); got keys: ${labels.sorted()}") {
              it
            }
        .isEqualTo("//external:com_github_pkg_errors")
  }

  // ------------------------------------------------------------------------
  // Regression coverage for https://github.com/Tinder/bazel-diff/issues/184
  // ------------------------------------------------------------------------
  // The user reported that a change inside an external repo C does not propagate to an
  // internal target A, when A depends on external repo B and B in turn depends on C. The
  // concrete report uses rules_python's pip_parse, where requirements.txt resolves into a
  // separate external repo for every package; the user's py_test rule (A) depends on @moto
  // (B), and @moto's BUILD targets depend on @cryptography (C). Bumping cryptography in
  // requirements.txt did not surface A as impacted.
  //
  // Root cause: BazelQueryService.queryBzlmodRepos synthesised a `//external:<repo>` target
  // per bzlmod-managed repo, hashed by repo *metadata* only. There was no rule_input edge
  // between those synthetic targets and no content hash of the underlying directory, so a
  // change inside @inner never reached @outer or //:consume.
  //
  // Fix: queryBzlmodRepos now (a) parses `bazel mod graph --output=json` for the dep edges
  // and emits `addRuleInput("//external:<dep_apparent>")` per direct bzlmod dep, and (b)
  // computes a recursive content hash of the directory for `local_repository`-rule repos
  // (which is what `local_path_override` lowers to) and attaches it as a synthetic
  // `_bazel_diff_content_hash` attribute. RuleHasher then follows the chain
  // //:consume -> //external:outer -> //external:inner during digest computation.
  //
  // Workspace `bzlmod_transitive_external`:
  //   //:consume         genrule, depends on @outer//:lib
  //   @outer//:lib       genrule in the outer module, depends on @inner//:data
  //   @inner//:data      filegroup in the inner module wrapping inner/data.txt
  //
  // Both `outer` and `inner` are real bzlmod modules brought in via `local_path_override`.
  //
  // This test asserts the post-fix behaviour: changing inner/data.txt with no extra flags
  // surfaces //:consume in the impacted-targets output.
  //
  // Requires Bazel 8.6.0+ for the `mod show_repo --output=streamed_proto` path that produces
  // the synthetic targets the fix mutates; same gating as
  // [testBzlmodShowRepoDetectsModuleBazelChanges].
  @Test
  fun testTransitiveExternalRepoChangeImpactsConsumer_regressionForIssue184() {
    val version = getBazelVersion()
    org.junit.Assume.assumeNotNull(version)
    val v = version!!
    val comparator =
        compareBy<Triple<Int, Int, Int>> { it.first }.thenBy { it.second }.thenBy { it.third }
    val hasModShowRepo = comparator.compare(v, Triple(8, 6, 0)) >= 0 && v != Triple(9, 0, 0)
    org.junit.Assume.assumeTrue(
        "Requires Bazel 8.6.0+ or 9.0.1+ (current: ${v.first}.${v.second}.${v.third})",
        hasModShowRepo)

    val workspaceA = copyTestWorkspace("bzlmod_transitive_external")
    val workspaceB = copyTestWorkspace("bzlmod_transitive_external")

    // Mutate the deepest, transitively-consumed file in B. Nothing in workspaceB's main
    // module changes; only inner/data.txt (which @inner re-exports via :data) is touched.
    val innerDataInB = File(workspaceB, "inner/data.txt")
    val original = innerDataInB.readText()
    val mutated = "world\n"
    assertThat(mutated != original).isEqualTo(true)
    innerDataInB.writeText(mutated)

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceA.absolutePath, "-b", "bazel", from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceB.absolutePath, "-b", "bazel", to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
    val consumerImpacted = impacted.any { it == "//:consume" || it == "@@//:consume" }
    assertThat(consumerImpacted)
        .transform(
            "//:consume should be impacted when inner/data.txt changes (transitive external dep " +
                "via @outer -> @inner per #184); got: $impacted") {
              it
            }
        .isEqualTo(true)
  }

  // ------------------------------------------------------------------------
  // Regression coverage for https://github.com/Tinder/bazel-diff/issues/197
  // ------------------------------------------------------------------------
  // The simple case (a main-repo target consuming a file from a single external repo) already
  // worked before this fix. The unfixed shape was the one @Ahajha called out in
  // https://github.com/Tinder/bazel-diff/issues/197#issuecomment-2616103349: a target inside
  // one external repo is re-wrapped by ANOTHER external repo, and the main repo consumes only
  // the wrapping repo. When the inner repo's source file changed, the main-repo consumer was
  // not reported as impacted unless the user manually enumerated every wrapping external repo
  // in --fineGrainedHashExternalRepos.
  //
  // The `wrapped_external_repo` fixture wires this up with bzlmod:
  //   inner_repo  -> filegroup wrapping data.txt
  //   middle_repo -> alias re-exporting @inner_repo//:all_files
  //   //:consumer -> genrule consuming @middle_repo//:wrapped
  //
  // The fix in `Modules.kt` auto-expands the fine-grained set with bzlmod modules that
  // transitively depend on a user-listed repo, by walking `bazel mod graph --output=json`.
  // Listing only `@inner_repo` is now enough: `@middle_repo` is added automatically, its
  // alias target is queried as a real rule, and its `actual` attribute carries the dep
  // chain down to `@inner_repo//:data.txt`.
  @Test
  fun testWrappedExternalRepoFileChangeImpactsMainConsumer_regressionForIssue197() {
    val workspaceA = copyTestWorkspace("wrapped_external_repo")
    val workspaceB = copyTestWorkspace("wrapped_external_repo")

    // Mutate only inner_repo/data.txt in B -- this is the source of truth.
    val innerDataInB = File(workspaceB, "inner_repo/data.txt")
    innerDataInB.writeText("version two\n")

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())

    // Only enumerate the source-of-truth repo. middle_repo is a transitive wrapper that
    // users don't expect to have to manually call out (they may not even know about it).
    val fineGrained = "@inner_repo"

    assertThat(
            cli.execute(
                "generate-hashes",
                "-w",
                workspaceA.absolutePath,
                "-b",
                "bazel",
                "--fineGrainedHashExternalRepos",
                fineGrained,
                from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "--fineGrainedHashExternalRepos",
                fineGrained,
                to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
    // Desired: //:consumer follows the dep chain through @middle_repo:wrapped ->
    // @inner_repo:all_files
    // -> data.txt and shows up as impacted. Current behaviour: only @inner_repo:* targets are
    // impacted; the chain stops at the unhashed @middle_repo "blob".
    val consumerImpacted = impacted.any { it == "//:consumer" || it == "@@//:consumer" }
    assertThat(consumerImpacted)
        .transform(
            "//:consumer should be impacted (chain: inner_repo/data.txt -> @inner_repo:all_files -> @middle_repo:wrapped -> //:consumer). Got impacted: $impacted") {
              it
            }
        .isEqualTo(true)
  }

  // ------------------------------------------------------------------------
  // Hermetic fine-grained external-repo hashing across workspaces
  // ------------------------------------------------------------------------
  // bazel-diff is only useful for distributed CI if the "from" hashes can be computed on one agent
  // and the "to" hashes on another. That requires generate-hashes to be hermetic: byte-identical
  // sources must hash identically regardless of the absolute workspace path or the per-workspace
  // Bazel output base (which Bazel derives from the workspace path and which, on real agents,
  // embeds machine-specific directories like
  // `/var/lib/buildkite-agent/builds/<agent>-<instance>/`).
  //
  // This test exercises the FINE-GRAINED external-repo path specifically: with
  // `--fineGrainedHashExternalRepos @inner_repo`, the individual targets inside @inner_repo are
  // hashed -- including the source file @inner_repo//:data.txt, which flows through
  // SourceFileHasher (the path made portable by #385). It checks out the same
  // `wrapped_external_repo`
  // fixture at two different absolute paths (each gets its own output base) and asserts the
  // fine-grained target hashes are identical.
  //
  // The synthetic //external:* blob targets are excluded from the comparison: they are a separate,
  // non-fine-grained representation built from `bazel mod show_repo`, and for local_path_override
  // repos their `path` attribute can carry an absolute, machine-specific location. That is a
  // distinct concern from the fine-grained hashing under test here.

  /**
   * Parses a generate-hashes output file and returns the inner target => hash map, handling both
   * the `{"hashes": {...}, "metadata": {...}}` (bzlmod) and the legacy flat `{...}` formats.
   */
  private fun readHashesMap(file: File): Map<String, Any> {
    val parsed = Gson().fromJson(file.readText(), com.google.gson.JsonObject::class.java)
    val hashesObj = if (parsed.has("hashes")) parsed.getAsJsonObject("hashes") else parsed
    return Gson().fromJson(hashesObj, object : TypeToken<Map<String, Any>>() {}.type)
  }

  @Test
  fun testFineGrainedExternalRepoHashesAreHermeticAcrossWorkspacePaths() {
    val version = getBazelVersion()
    org.junit.Assume.assumeNotNull(version)
    val v = version!!
    val comparator =
        compareBy<Triple<Int, Int, Int>> { it.first }.thenBy { it.second }.thenBy { it.third }
    val hasModShowRepo = comparator.compare(v, Triple(8, 6, 0)) >= 0 && v != Triple(9, 0, 0)
    org.junit.Assume.assumeTrue(
        "Requires Bazel 8.6.0+ or 9.0.1+ (current: ${v.first}.${v.second}.${v.third})",
        hasModShowRepo)

    // Two checkouts of byte-identical sources at different absolute paths -- standing in for the
    // same commit on two different CI agents.
    val machineX = copyTestWorkspace("wrapped_external_repo")
    val machineY = copyTestWorkspace("wrapped_external_repo")

    val outputDir = temp.newFolder()
    val hashesX = File(outputDir, "fine_grained_machine_x.json")
    val hashesY = File(outputDir, "fine_grained_machine_y.json")

    val fineGrained = "@inner_repo"
    val cli = CommandLine(BazelDiff())
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w",
                machineX.absolutePath,
                "-b",
                "bazel",
                "--fineGrainedHashExternalRepos",
                fineGrained,
                hashesX.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes",
                "-w",
                machineY.absolutePath,
                "-b",
                "bazel",
                "--fineGrainedHashExternalRepos",
                fineGrained,
                hashesY.absolutePath))
        .isEqualTo(0)

    val mapX = readHashesMap(hashesX).filterKeys { !it.startsWith("//external:") }
    val mapY = readHashesMap(hashesY).filterKeys { !it.startsWith("//external:") }

    // Sanity: the fine-grained external repo's source file must actually be in the hashed set, so
    // an empty-vs-empty comparison can't masquerade as success and we know the path under test is
    // covered.
    assertThat(mapX.keys.any { it.contains("inner_repo") && it.contains("data.txt") })
        .transform(
            "expected a fine-grained @inner_repo source target (data.txt) in hashes; got keys: ${mapX.keys.sorted()}") {
              it
            }
        .isEqualTo(true)

    assertTargetsMatch(mapX.keys, mapY.keys, "fine-grained hermetic hashes (key sets)")
    assertThat(mapX)
        .transform(
            "fine-grained external-repo hashes computed at different workspace paths must be identical") {
              it
            }
        .isEqualTo(mapY)
  }

  // ------------------------------------------------------------------------
  // Regression coverage for https://github.com/Tinder/bazel-diff/issues/365
  // ------------------------------------------------------------------------
  // The user reported that after upgrading bazel-diff (12.0.0 -> 25.0.0), ADDING (or editing) a
  // `.bzl` macro and `load()`ing it -- even in an otherwise-empty BUILD file -- caused EVERY
  // target in the repository to be reported as impacted, not just the targets that load the macro.
  //
  // Root cause was that the #259/#227 fix rolled the union of EVERY main-repo `.bzl` file's digest
  // into a single workspace-wide seed that was then mixed into every target's hash, so introducing
  // a brand-new, unrelated `.bzl` flipped the seed and changed the hash of completely unrelated
  // targets -- e.g. native genrules in another package that load nothing.
  //
  // Fix: BuildGraphHasher.createPackageBzlSeeds attributes each `.bzl` digest to the package whose
  // BUILD `load()`s it, and each target mixes in only its own package's seed. A macro added in a
  // new package no longer perturbs targets that never load it, while #259/#227 stays fixed because
  // a `.bzl` edit still re-hashes every target in the packages that load it.
  //
  // Fixture `bzl_seed_overtrigger` has a single package `//pkg` with two native genrules
  // (`//pkg:a`, `//pkg:b`) that load no `.bzl`. The scenario adds a NEW package `//macros` whose
  // BUILD only does `load("//macros:defs.bzl", "my_macro")` (an empty load -- exactly the shape
  // from the issue). The new `//macros:*` source files are legitimately new, but `//pkg:a` /
  // `//pkg:b` must not be affected by a macro they never load.

  /**
   * Copies the fixture twice, adds an unrelated macro + empty-`load()` BUILD to B, runs the full
   * generate-hashes / get-impacted-targets flow, and returns the impacted-targets set.
   */
  private fun runIssue365MacroAddScenario(): Set<String> {
    val workspaceA = copyTestWorkspace("bzl_seed_overtrigger")
    val workspaceB = copyTestWorkspace("bzl_seed_overtrigger")

    // In B, add a brand-new package that defines a macro and `load()`s it in an otherwise-empty
    // BUILD file. Nothing in //pkg changes.
    File(workspaceB, "macros").mkdirs()
    File(workspaceB, "macros/defs.bzl")
        .writeText(
            "def my_macro(name, **kwargs):\n" +
                "    native.genrule(name = name, outs = [name + \".txt\"], cmd = \"echo hi > \$@\", **kwargs)\n")
    File(workspaceB, "macros/BUILD").writeText("load(\"//macros:defs.bzl\", \"my_macro\")\n")

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceA.absolutePath, "-b", "bazel", from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceB.absolutePath, "-b", "bazel", to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    return impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()
  }

  // Post-fix invariant for #365: adding an unrelated macro must NOT impact //pkg:a or //pkg:b.
  // BuildGraphHasher.createPackageBzlSeeds now attributes each `.bzl` digest to the package that
  // `load()`s it, so the new `//macros` package's macro does not perturb the `//pkg` targets that
  // load nothing.
  @Test
  fun testAddingUnrelatedMacroDoesNotImpactExistingTargets_reproducerForIssue365() {
    val impacted = runIssue365MacroAddScenario()
    val unrelatedImpacted =
        impacted.filter {
          it == "//pkg:a" || it == "@@//pkg:a" || it == "//pkg:b" || it == "@@//pkg:b"
        }
    assertThat(unrelatedImpacted.isEmpty())
        .transform(
            "Adding an unrelated macro in //macros must not impact //pkg:a or //pkg:b; got: $impacted") {
              it
            }
        .isEqualTo(true)
  }

  // Editing an EXISTING widely-loaded rule `.bzl` must be scoped too, not just newly-added macros
  // (#365). Fixture `rule_bzl_overtrigger`: `//app` loads `//defs:thing.bzl` for one target
  // (`//app:t`) alongside unrelated native targets (`:native_gen`, `:fg`, `:data.txt`). Editing
  // `thing.bzl` must impact `//app:t` (its instantiation-stack seed + `$rule_implementation_hash`)
  // but MUST NOT impact the native targets that only share its package.
  @Test
  fun testEditingRuleBzlDoesNotOverInvalidateSamePackageTargets() {
    val workspaceA = copyTestWorkspace("rule_bzl_overtrigger")
    val workspaceB = copyTestWorkspace("rule_bzl_overtrigger")

    // Change thing.bzl's rule impl without changing any emitted attribute of `thing`.
    val bzlInB = File(workspaceB, "defs/thing.bzl")
    val original = bzlInB.readText()
    val mutated =
        original.replace(
            "ctx.actions.write(out, \"thing\")", "ctx.actions.write(out, \"thing\")  # edited")
    assertThat(mutated != original).isEqualTo(true)
    bzlInB.writeText(mutated)

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impactedTargetsOutput = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceA.absolutePath, "-b", "bazel", from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceB.absolutePath, "-b", "bazel", to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                impactedTargetsOutput.absolutePath))
        .isEqualTo(0)

    val impacted = impactedTargetsOutput.readLines().filter { it.isNotBlank() }.toSet()

    // The target that instantiates the rule from thing.bzl must be impacted.
    assertThat(impacted.any { it.endsWith("//app:t") })
        .transform("//app:t should be impacted by a thing.bzl edit; got: $impacted") { it }
        .isEqualTo(true)

    // Unrelated native targets sharing the package must NOT be impacted (the over-invalidation).
    val overInvalidated =
        impacted.filter {
          it.endsWith("//app:native_gen") ||
              it.endsWith("//app:g.txt") ||
              it.endsWith("//app:fg") ||
              it.endsWith("//app:data.txt")
        }
    assertThat(overInvalidated.isEmpty())
        .transform(
            "Editing thing.bzl must not impact unrelated //app targets; over-invalidated: $overInvalidated") {
              it
            }
        .isEqualTo(true)
  }

  // Cross-file macro/rule: `//app` loads `//defs:macro.bzl`, which loads `//defs:rule.bzl`, for one
  // target (`//app:s`). Editing EITHER file must impact `//app:s` -- the macro via its
  // instantiation-stack seed, the rule via `$rule_implementation_hash` -- so the scoping does not
  // under-invalidate. `//app:unrelated_gen` (a sibling native target) must stay out.
  private fun crossFileImpacted(mutate: (File) -> Unit): Set<String> {
    val workspaceA = copyTestWorkspace("cross_file_macro_rule")
    val workspaceB = copyTestWorkspace("cross_file_macro_rule")
    mutate(workspaceB)

    val outputDir = temp.newFolder()
    val from = File(outputDir, "starting_hashes.json")
    val to = File(outputDir, "final_hashes.json")
    val impacted = File(outputDir, "impacted_targets.txt")

    val cli = CommandLine(BazelDiff())
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceA.absolutePath, "-b", "bazel", from.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "generate-hashes", "-w", workspaceB.absolutePath, "-b", "bazel", to.absolutePath))
        .isEqualTo(0)
    assertThat(
            cli.execute(
                "get-impacted-targets",
                "-w",
                workspaceB.absolutePath,
                "-b",
                "bazel",
                "-sh",
                from.absolutePath,
                "-fh",
                to.absolutePath,
                "-o",
                impacted.absolutePath))
        .isEqualTo(0)
    return impacted.readLines().filter { it.isNotBlank() }.toSet()
  }

  private fun assertCrossFileConsumerScoped(impacted: Set<String>, edited: String) {
    assertThat(impacted.any { it.endsWith("//app:s") })
        .transform("//app:s should be impacted by the $edited edit; got: $impacted") { it }
        .isEqualTo(true)
    assertThat(impacted.none { it.endsWith("//app:unrelated_gen") || it.endsWith("//app:u.txt") })
        .transform("$edited edit must not impact //app:unrelated_gen; got: $impacted") { it }
        .isEqualTo(true)
  }

  @Test
  fun testEditingCrossFileMacroImpactsConsumerNotSiblings() {
    val impacted = crossFileImpacted { ws ->
      val f = File(ws, "defs/macro.bzl")
      f.writeText(
          f.readText()
              .replace(
                  "def split(name, **kwargs):",
                  "def split(name, **kwargs):\n    print(\"split: \" + name)"))
    }
    assertCrossFileConsumerScoped(impacted, "defs/macro.bzl")
  }

  @Test
  fun testEditingCrossFileRuleImpactsConsumerNotSiblings() {
    val impacted = crossFileImpacted { ws ->
      val f = File(ws, "defs/rule.bzl")
      f.writeText(
          f.readText()
              .replace(
                  "ctx.actions.write(out, \"split\")",
                  "ctx.actions.write(out, \"split\")  # edited"))
    }
    assertCrossFileConsumerScoped(impacted, "defs/rule.bzl")
  }

  /**
   * Reads a `generate-hashes` output file into a flat `target -> hash` map, tolerating both the
   * bzlmod "new format" (`{"hashes": {...}, "metadata": {...}}`) and the legacy flat format
   * (`{target: hash}`) emitted for non-bzlmod workspaces.
   */
  @Suppress("UNCHECKED_CAST")
  private fun readTargetHashes(file: File): Map<String, Any> {
    val root: Map<String, Any> =
        Gson().fromJson(file.readText(), object : TypeToken<Map<String, Any>>() {}.type)
    return (root["hashes"] as? Map<String, Any>) ?: root
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
