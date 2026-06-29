package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.containsExactlyInAnyOrder
import assertk.assertions.isEqualTo
import com.bazel_diff.SilentLogger
import com.bazel_diff.bazel.BazelModService
import com.bazel_diff.hash.TargetHash
import com.bazel_diff.interactor.HashFileData
import com.bazel_diff.log.Logger
import com.google.gson.GsonBuilder
import org.junit.Rule
import org.junit.Test
import org.koin.dsl.module
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule
import org.mockito.junit.MockitoJUnit
import org.mockito.kotlin.doReturn
import org.mockito.kotlin.mock

class ImpactedTargetsServiceTest : KoinTest {
  @get:Rule val mockitoRule = MockitoJUnit.rule()

  private val bazelModService: BazelModService = mock { on { isBzlmodEnabled } doReturn false }

  @get:Rule
  val koinTestRule =
      KoinTestRule.create {
        modules(
            module {
              single<Logger> { SilentLogger }
              single { GsonBuilder().disableHtmlEscaping().create() }
              single { bazelModService }
            })
      }

  /** [GitClient] that treats every revision as already a SHA (identity resolution). */
  private class IdentityGitClient : GitClient {
    override fun fetch() = Unit

    override fun resolveSha(revision: String) = revision

    override fun checkout(revision: String) = Unit
  }

  /**
   * [HashProvider] returning canned data. By default the module-change workspace path is treated as
   * an error (the pure hash-diff path must not touch the workspace); set [allowWorkspaceAt] to
   * record
   * + run it instead.
   */
  private class FakeHashProvider(
      private val byRev: Map<String, HashFileData>,
      private val allowWorkspaceAt: Boolean = false,
  ) : HashProvider {
    val workspaceAtCalls = mutableListOf<String>()

    override fun getHashes(sha: String): HashFileData =
        byRev[sha] ?: error("no canned hashes for $sha")

    override fun <T> withWorkspaceAt(sha: String, block: () -> T): T {
      if (!allowWorkspaceAt)
          error("withWorkspaceAt should not be called on the pure hash-diff path")
      workspaceAtCalls += sha
      return block()
    }
  }

  @Test
  fun computesChangedAndAddedTargets() {
    val from =
        HashFileData(
            mapOf(
                "//:a" to TargetHash("Rule", "h1", "d1"), "//:b" to TargetHash("Rule", "h1", "d1")),
            null)
    val to =
        HashFileData(
            mapOf(
                "//:a" to TargetHash("Rule", "h1", "d1"), // unchanged
                "//:b" to TargetHash("Rule", "h2", "d2"), // changed
                "//:c" to TargetHash("Rule", "hx", "dx")), // added
            null)
    val service =
        ImpactedTargetsService(
            IdentityGitClient(), FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to)))

    val result = service.getImpactedTargets("from-sha", "to-sha", null)

    assertThat(result.from).isEqualTo("from-sha")
    assertThat(result.to).isEqualTo("to-sha")
    assertThat(result.impactedTargets).containsExactlyInAnyOrder("//:b", "//:c")
  }

  @Test
  fun targetTypeFilterIsApplied() {
    val from = HashFileData(mapOf("//:r" to TargetHash("Rule", "h1", "d1")), null)
    val to =
        HashFileData(
            mapOf(
                "//:r" to TargetHash("Rule", "h2", "d2"),
                "//:s" to TargetHash("SourceFile", "hs", "ds")),
            null)
    val service =
        ImpactedTargetsService(IdentityGitClient(), FakeHashProvider(mapOf("a" to from, "b" to to)))

    val result = service.getImpactedTargets("a", "b", setOf("Rule"))

    assertThat(result.impactedTargets).isEqualTo(listOf("//:r"))
  }

  @Test
  fun moduleGraphChangePinsWorkspaceToTo() {
    // Differing module-graph JSON forces the live-query path, which must pin the working tree to
    // the
    // `to` revision via withWorkspaceAt. Both blobs parse to empty graphs, so no BazelQueryService
    // is
    // needed and the interactor falls back to the plain hash diff.
    val from = HashFileData(mapOf("//:a" to TargetHash("Rule", "h1", "d1")), "graph-v1")
    val to = HashFileData(mapOf("//:a" to TargetHash("Rule", "h2", "d2")), "graph-v2")
    val provider =
        FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to), allowWorkspaceAt = true)
    val service = ImpactedTargetsService(IdentityGitClient(), provider)

    val result = service.getImpactedTargets("from-sha", "to-sha", null)

    assertThat(result.impactedTargets).isEqualTo(listOf("//:a"))
    assertThat(provider.workspaceAtCalls).isEqualTo(listOf("to-sha"))
  }
}
