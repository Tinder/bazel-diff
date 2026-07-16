package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.containsExactly
import assertk.assertions.containsExactlyInAnyOrder
import assertk.assertions.isEqualTo
import assertk.assertions.isGreaterThan
import assertk.assertions.isGreaterThanOrEqualTo
import assertk.assertions.isNull
import com.bazel_diff.SilentLogger
import com.bazel_diff.bazel.BazelModService
import com.bazel_diff.hash.TargetHash
import com.bazel_diff.interactor.HashFileData
import com.bazel_diff.interactor.ImpactedTargetWithDistance
import com.bazel_diff.log.Logger
import com.google.gson.GsonBuilder
import java.nio.file.Path
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

  /** [GitClient] whose [missingUntilFetch] revisions resolve only after [fetch] has run once. */
  private class RefetchGitClient(private val missingUntilFetch: Set<String>) : GitClient {
    var fetchCount = 0
    private var fetched = false

    override fun fetch() {
      fetchCount++
      fetched = true
    }

    override fun resolveSha(revision: String): String {
      if (!fetched && revision in missingUntilFetch) throw MissingRevisionException(revision)
      return revision
    }

    override fun checkout(revision: String) = Unit
  }

  /**
   * [GitClient] that fails the first resolve of [flaky] and succeeds on the next WITHOUT a fetch --
   * models another request having refetched while this one waited on the fetch lock.
   */
  private class ResolvesOnSecondAttemptGitClient(private val flaky: String) : GitClient {
    var fetchCount = 0
    private var attempts = 0

    override fun fetch() {
      fetchCount++
    }

    override fun resolveSha(revision: String): String {
      if (revision == flaky && ++attempts == 1) throw MissingRevisionException(revision)
      return revision
    }

    override fun checkout(revision: String) = Unit
  }

  /** [GitClient] whose [missing] revisions never resolve, even after a broad or targeted fetch. */
  private class AlwaysMissingGitClient(private val missing: Set<String>) : GitClient {
    var fetchCount = 0
    var fetchRevisionCount = 0

    override fun fetch() {
      fetchCount++
    }

    override fun fetchRevision(revision: String): Boolean {
      fetchRevisionCount++
      return false
    }

    override fun resolveSha(revision: String): String {
      if (revision in missing) throw MissingRevisionException(revision)
      return revision
    }

    override fun checkout(revision: String) = Unit
  }

  /**
   * [GitClient] modeling a PR-head SHA: a broad [fetch] never brings [prHead] in (it is on no
   * fetched ref); only a targeted [fetchRevision] does.
   */
  private class DirectFetchGitClient(private val prHead: String) : GitClient {
    var fetchCount = 0
    var fetchRevisionCount = 0
    private var directlyFetched = false

    override fun fetch() {
      fetchCount++
    }

    override fun fetchRevision(revision: String): Boolean {
      fetchRevisionCount++
      if (revision == prHead) {
        directlyFetched = true
        return true
      }
      return false
    }

    override fun resolveSha(revision: String): String {
      if (revision == prHead && !directlyFetched) throw MissingRevisionException(revision)
      return revision
    }

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
    val modifiedFilepathsByRev = mutableMapOf<String, Set<Path>>()

    override fun getHashes(
        sha: String,
        modifiedFilepaths: Set<Path>,
        profiler: QueryProfiler?
    ): HashFileData {
      modifiedFilepathsByRev[sha] = modifiedFilepaths
      // Mirror HashService: each retrieval reports itself to the profiler (canned data = a "hit").
      profiler?.recordHashRetrieval(
          HashRetrievalProfile(sha, cacheHit = true, durationMillis = 1, cacheReadMillis = 1))
      return byRev[sha] ?: error("no canned hashes for $sha")
    }

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
  fun scopesBothRevisionsWithTheSameModifiedFilepaths() {
    // The identical modified-filepaths scope must reach BOTH revisions -- that symmetry is what
    // keeps
    // a content-scoped diff correct (an unchanged, unlisted file is content-skipped on both sides).
    val from = HashFileData(mapOf("//:a" to TargetHash("Rule", "h1", "d1")), null)
    val to = HashFileData(mapOf("//:a" to TargetHash("Rule", "h2", "d2")), null)
    val provider = FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to))
    val service = ImpactedTargetsService(IdentityGitClient(), provider)
    val modified = setOf(Path.of("pkg/A.kt"), Path.of("pkg/B.kt"))

    val result = service.getImpactedTargets("from-sha", "to-sha", null, modified)

    assertThat(result.impactedTargets).isEqualTo(listOf("//:a"))
    assertThat(provider.modifiedFilepathsByRev["from-sha"]).isEqualTo(modified)
    assertThat(provider.modifiedFilepathsByRev["to-sha"]).isEqualTo(modified)
  }

  @Test
  fun scopesBothRevisionsForDistancesToo() {
    val from = HashFileData(mapOf("//:a" to TargetHash("Rule", "h1", "d1")), null)
    val to =
        HashFileData(
            mapOf("//:a" to TargetHash("Rule", "h2", "d2")),
            null,
            depEdges = mapOf("//:a" to emptyList()))
    val provider = FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to))
    val service = ImpactedTargetsService(IdentityGitClient(), provider, depsTracked = true)
    val modified = setOf(Path.of("pkg/A.kt"))

    service.getImpactedTargetsWithDistances("from-sha", "to-sha", null, modified)

    assertThat(provider.modifiedFilepathsByRev["from-sha"]).isEqualTo(modified)
    assertThat(provider.modifiedFilepathsByRev["to-sha"]).isEqualTo(modified)
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

  @Test
  fun computesDistancesFromToRevisionDepEdges() {
    // //a:lib changed its own content (directHash) -> DIRECT, distance 0.
    // //b:lib only changed transitively (hash differs, directHash same) and depends on //a:lib ->
    // INDIRECT, one hop across a package boundary -> targetDistance 1, packageDistance 1.
    val from =
        HashFileData(
            mapOf(
                "//a:lib" to TargetHash("Rule", "h1", "d1"),
                "//b:lib" to TargetHash("Rule", "hb1", "db1")),
            null)
    val to =
        HashFileData(
            mapOf(
                "//a:lib" to TargetHash("Rule", "h2", "d2"),
                "//b:lib" to TargetHash("Rule", "hb2", "db1")),
            null,
            depEdges = mapOf("//b:lib" to listOf("//a:lib"), "//a:lib" to emptyList()))
    val service =
        ImpactedTargetsService(
            IdentityGitClient(),
            FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to)),
            depsTracked = true)

    val result = service.getImpactedTargetsWithDistances("from-sha", "to-sha", null)

    assertThat(result.from).isEqualTo("from-sha")
    assertThat(result.to).isEqualTo("to-sha")
    assertThat(result.impactedTargets)
        .containsExactly(
            ImpactedTargetWithDistance("//a:lib", 0, 0),
            ImpactedTargetWithDistance("//b:lib", 1, 1))
  }

  @Test
  fun distancesThrowWhenDepsNotTracked() {
    val from = HashFileData(mapOf("//:a" to TargetHash("Rule", "h1", "d1")), null)
    val to = HashFileData(mapOf("//:a" to TargetHash("Rule", "h2", "d2")), null)
    val service =
        ImpactedTargetsService(
            IdentityGitClient(),
            FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to)),
            depsTracked = false)

    org.junit.Assert.assertThrows(DistancesUnavailableException::class.java) {
      service.getImpactedTargetsWithDistances("from-sha", "to-sha", null)
    }
  }

  @Test
  fun distancesModuleGraphChangePinsWorkspaceToTo() {
    // Same forced live-query path as the non-distance test, but through the distances method.
    val from = HashFileData(mapOf("//:a" to TargetHash("Rule", "h1", "d1")), "graph-v1")
    val to =
        HashFileData(
            mapOf("//:a" to TargetHash("Rule", "h2", "d2")),
            "graph-v2",
            depEdges = mapOf("//:a" to emptyList()))
    val provider =
        FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to), allowWorkspaceAt = true)
    val service = ImpactedTargetsService(IdentityGitClient(), provider, depsTracked = true)

    val result =
        service.getImpactedTargetsWithDistances(
            "from-sha", "to-sha", null, profiler = QueryProfiler())

    assertThat(result.impactedTargets).containsExactly(ImpactedTargetWithDistance("//:a", 0, 0))
    assertThat(provider.workspaceAtCalls).isEqualTo(listOf("to-sha"))
    // The live-rdeps diff path is called out in the profile so a slow diff is attributable to it.
    assertThat(result.profile!!.diffModuleGraphChanged).isEqualTo(true)
  }

  @Test
  fun profilerAttachesQueryAndMemoryProfiles() {
    val from = HashFileData(mapOf("//:a" to TargetHash("Rule", "h1", "d1")), null)
    val to = HashFileData(mapOf("//:a" to TargetHash("Rule", "h2", "d2")), null)
    val service =
        ImpactedTargetsService(
            IdentityGitClient(), FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to)))

    val result = service.getImpactedTargets("from-sha", "to-sha", null, profiler = QueryProfiler())

    val profile = result.profile!!
    // Both sides' hash retrievals were recorded, in order, with their cache-hit flags.
    assertThat(profile.hashRetrievals.map { it.sha }).isEqualTo(listOf("from-sha", "to-sha"))
    assertThat(profile.hashRetrievals.map { it.cacheHit }).isEqualTo(listOf(true, true))
    assertThat(profile.totalDurationMillis).isGreaterThanOrEqualTo(0)
    assertThat(profile.resolveRevisionsDurationMillis).isGreaterThanOrEqualTo(0)
    assertThat(profile.diffDurationMillis).isGreaterThanOrEqualTo(0)
    // Identical (null) module graphs on both sides: the pure hash-diff path ran.
    assertThat(profile.diffModuleGraphChanged).isEqualTo(false)
    val memory = result.memoryProfile!!
    assertThat(memory.heapMaxBytes).isGreaterThan(0)
    assertThat(memory.heapUsedAfterBytes - memory.heapUsedBeforeBytes)
        .isEqualTo(memory.heapUsedDeltaBytes)
  }

  @Test
  fun noProfilerMeansNoProfileOnTheResult() {
    val from = HashFileData(mapOf("//:a" to TargetHash("Rule", "h1", "d1")), null)
    val to = HashFileData(mapOf("//:a" to TargetHash("Rule", "h2", "d2")), null)
    val service =
        ImpactedTargetsService(
            IdentityGitClient(), FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to)))

    val result = service.getImpactedTargets("from-sha", "to-sha", null)

    assertThat(result.profile).isNull()
    assertThat(result.memoryProfile).isNull()
  }

  @Test
  fun distancesResultCarriesProfilesToo() {
    val from = HashFileData(mapOf("//:a" to TargetHash("Rule", "h1", "d1")), null)
    val to =
        HashFileData(
            mapOf("//:a" to TargetHash("Rule", "h2", "d2")),
            null,
            depEdges = mapOf("//:a" to emptyList()))
    val service =
        ImpactedTargetsService(
            IdentityGitClient(),
            FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to)),
            depsTracked = true)

    val result =
        service.getImpactedTargetsWithDistances(
            "from-sha", "to-sha", null, profiler = QueryProfiler())

    assertThat(result.profile!!.hashRetrievals.map { it.sha })
        .isEqualTo(listOf("from-sha", "to-sha"))
    assertThat(result.memoryProfile!!.heapMaxBytes).isGreaterThan(0)
  }

  @Test
  fun refetchesOnceWhenRevisionMissingThenResolves() {
    // The `to` commit isn't in the local clone yet (it landed after the last fetch); the service
    // must fetch on demand and retry rather than surfacing the miss as a 400.
    val from = HashFileData(mapOf("//:a" to TargetHash("Rule", "h1", "d1")), null)
    val to = HashFileData(mapOf("//:a" to TargetHash("Rule", "h2", "d2")), null)
    val git = RefetchGitClient(missingUntilFetch = setOf("to-sha"))
    val service =
        ImpactedTargetsService(git, FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to)))

    val result = service.getImpactedTargets("from-sha", "to-sha", null)

    assertThat(result.impactedTargets).isEqualTo(listOf("//:a"))
    assertThat(git.fetchCount).isEqualTo(1)
  }

  @Test
  fun skipsRefetchWhenRevisionAppearsUnderTheLock() {
    // A concurrent request already refetched: the double-check re-resolve under the lock succeeds,
    // so this request must not issue its own redundant fetch.
    val from = HashFileData(mapOf("//:a" to TargetHash("Rule", "h1", "d1")), null)
    val to = HashFileData(mapOf("//:a" to TargetHash("Rule", "h2", "d2")), null)
    val git = ResolvesOnSecondAttemptGitClient(flaky = "to-sha")
    val service =
        ImpactedTargetsService(git, FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to)))

    val result = service.getImpactedTargets("from-sha", "to-sha", null)

    assertThat(result.impactedTargets).isEqualTo(listOf("//:a"))
    assertThat(git.fetchCount).isEqualTo(0)
  }

  @Test
  fun propagatesMissingRevisionAfterRefetchStillFails() {
    // A genuinely unknown revision: a broad refetch AND a targeted by-SHA fetch are both attempted,
    // then the miss propagates (the HTTP layer maps it to a 400).
    val git = AlwaysMissingGitClient(missing = setOf("to-sha"))
    val service = ImpactedTargetsService(git, FakeHashProvider(emptyMap()))

    val ex =
        org.junit.Assert.assertThrows(MissingRevisionException::class.java) {
          service.getImpactedTargets("from-sha", "to-sha", null)
        }
    assertThat(ex.revision).isEqualTo("to-sha")
    assertThat(git.fetchCount).isEqualTo(1)
    assertThat(git.fetchRevisionCount).isEqualTo(1)
  }

  @Test
  fun fetchesUnreachableRevisionDirectlyWhenBroadFetchDoesNotBringItIn() {
    // The `to` SHA is a PR-head commit reachable from no fetched ref, so a broad `git fetch --all`
    // never brings it in. Rather than 400, the service must fall back to a targeted by-SHA fetch.
    val from = HashFileData(mapOf("//:a" to TargetHash("Rule", "h1", "d1")), null)
    val to = HashFileData(mapOf("//:a" to TargetHash("Rule", "h2", "d2")), null)
    val git = DirectFetchGitClient(prHead = "to-sha")
    val service =
        ImpactedTargetsService(git, FakeHashProvider(mapOf("from-sha" to from, "to-sha" to to)))

    val result = service.getImpactedTargets("from-sha", "to-sha", null)

    assertThat(result.impactedTargets).isEqualTo(listOf("//:a"))
    assertThat(git.fetchCount).isEqualTo(1) // broad fetch tried first
    assertThat(git.fetchRevisionCount).isEqualTo(1) // then the targeted by-SHA fetch
  }
}
