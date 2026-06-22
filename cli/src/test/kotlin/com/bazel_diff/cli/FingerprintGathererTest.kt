package com.bazel_diff.cli

import assertk.assertThat
import assertk.assertions.containsAll
import assertk.assertions.isEqualTo
import java.io.File
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class FingerprintGathererTest {
  @get:Rule val temp: TemporaryFolder = TemporaryFolder()

  @Test
  fun canonicalizeFlagsSortsSetsAndJoinsLists() {
    val flags =
        FingerprintGatherer.canonicalizeFlags(
            bazelStartupOptions = listOf("--a", "--b"),
            bazelCommandOptions = listOf("--c"),
            cqueryCommandOptions = emptyList(),
            useCquery = true,
            cqueryExpression = "deps(//...)",
            includeTargetType = false,
            targetType = setOf("Rule", "GeneratedFile"),
            fineGrainedHashExternalRepos = setOf("maven", "abc"),
            ignoredRuleHashingAttributes = emptySet(),
            excludeExternalTargets = true,
            keepGoing = false,
        )
    assertThat(flags["bazelStartupOptions"]).isEqualTo("--a --b")
    assertThat(flags["bazelCommandOptions"]).isEqualTo("--c")
    assertThat(flags["useCquery"]).isEqualTo("true")
    assertThat(flags["cqueryExpression"]).isEqualTo("deps(//...)")
    // sets are sorted + comma-joined for determinism
    assertThat(flags["targetType"]).isEqualTo("GeneratedFile,Rule")
    assertThat(flags["fineGrainedHashExternalRepos"]).isEqualTo("abc,maven")
    assertThat(flags["excludeExternalTargets"]).isEqualTo("true")
    assertThat(flags["keepGoing"]).isEqualTo("false")
  }

  @Test
  fun canonicalizeFlagsIsOrderIndependentForSets() {
    fun build(tt: Set<String>) =
        FingerprintGatherer.canonicalizeFlags(
            emptyList(), emptyList(), emptyList(), false, null, false, tt,
            emptySet(), emptySet(), false, true)
    assertThat(build(linkedSetOf("a", "b", "c"))).isEqualTo(build(linkedSetOf("c", "a", "b")))
  }

  private fun fakeBazel(label: String): File =
      File(temp.root, "bazel").apply {
        writeText("#!/bin/sh\necho '$label'\n")
        setExecutable(true)
      }

  @Test
  fun gatherReadsLockBazelrcImportsAndBazelVersion() {
    val ws = temp.newFolder("ws")
    File(ws, "MODULE.bazel.lock").writeText("lockbytes")
    // .bazelrc that imports another rc via %workspace%
    File(ws, ".bazelrc").writeText("common --x\nimport %workspace%/ci.bazelrc\n")
    File(ws, "ci.bazelrc").writeText("build --y")
    val bazel = fakeBazel("Build label: 8.5.1")

    val flags = mapOf("useCquery" to "false")
    val inputs =
        FingerprintGatherer.gather(ws.toPath(), bazel.toPath(), "26.0.1", flags)

    assertThat(inputs.bazelVersion).isEqualTo("8.5.1")
    assertThat(inputs.bazelDiffVersion).isEqualTo("26.0.1")
    assertThat(String(inputs.moduleLockContent!!)).isEqualTo("lockbytes")
    assertThat(inputs.flags).isEqualTo(flags)
    // both the root rc and the imported rc are captured
    assertThat(inputs.bazelrcContents.keys).containsAll(".bazelrc", "ci.bazelrc")
  }

  @Test
  fun gatherHandlesMissingFilesAndUnknownBazel() {
    val ws = temp.newFolder("empty-ws") // no MODULE.bazel.lock, no .bazelrc
    val missingBazel = File(temp.root, "no-bazel").toPath()
    val inputs = FingerprintGatherer.gather(ws.toPath(), missingBazel, "26.0.1", emptyMap())
    assertThat(inputs.moduleLockContent).isEqualTo(null)
    assertThat(inputs.bazelrcContents.isEmpty()).isEqualTo(true)
    // a bazel binary that can't run yields the "unknown" sentinel, not a crash
    assertThat(inputs.bazelVersion).isEqualTo("unknown")
  }
}
