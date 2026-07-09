package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.containsOnly
import assertk.assertions.hasSize
import com.bazel_diff.SilentLogger
import com.bazel_diff.log.Logger
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Before
import org.junit.Test
import org.koin.core.context.startKoin
import org.koin.core.context.stopKoin
import org.koin.dsl.module
import org.koin.test.KoinTest
import org.mockito.kotlin.mock
import org.mockito.kotlin.never
import org.mockito.kotlin.times
import org.mockito.kotlin.verify
import org.mockito.kotlin.whenever

class BazelClientTest : KoinTest {

  private val queryService: BazelQueryService = mock()
  private val modService: BazelModService = mock()

  @Before
  fun setUp() {
    startKoin {
      modules(
          module {
            single<Logger> { SilentLogger }
            single { queryService }
            single { modService }
          })
    }
  }

  @After
  fun tearDown() {
    stopKoin()
  }

  @Test
  fun queryAllTargets_bzlmodEnabled_bazel9_includesBzlmodRepos() {
    runBlocking {
      val mainTarget = mockRuleTarget("//src:lib")
      val bzlmodTarget = mockRuleTarget("//external:rules_java~")

      whenever(modService.isBzlmodEnabled).thenReturn(true)
      whenever(queryService.canUseBzlmodShowRepo).thenReturn(true)
      whenever(queryService.query("'//...:all-targets'")).thenReturn(listOf(mainTarget))
      whenever(queryService.queryBzlmodRepos()).thenReturn(listOf(bzlmodTarget))

      val client =
          BazelClient(
              useCquery = false,
              cqueryExpression = null,
              fineGrainedHashExternalRepos = emptySet(),
              excludeExternalTargets = false,
              excludeTargetsQuery = null)

      val targets = client.queryAllTargets()

      assertThat(targets).hasSize(2)
      assertThat(targets.map { it.name }).containsOnly("//src:lib", "//external:rules_java~")
      verify(queryService).queryBzlmodRepos()
    }
  }

  @Test
  fun queryAllTargets_bzlmodEnabled_oldBazel_skipsBzlmodRepos() {
    runBlocking {
      val mainTarget = mockRuleTarget("//src:lib")

      whenever(modService.isBzlmodEnabled).thenReturn(true)
      whenever(queryService.canUseBzlmodShowRepo).thenReturn(false)
      whenever(queryService.query("'//...:all-targets'")).thenReturn(listOf(mainTarget))

      val client =
          BazelClient(
              useCquery = false,
              cqueryExpression = null,
              fineGrainedHashExternalRepos = emptySet(),
              excludeExternalTargets = false,
              excludeTargetsQuery = null)

      val targets = client.queryAllTargets()

      assertThat(targets.map { it.name }).containsOnly("//src:lib")
      verify(queryService, never()).queryBzlmodRepos()
    }
  }

  @Test
  fun queryAllTargets_bzlmodDisabled_skipsBzlmodRepos() {
    runBlocking {
      val mainTarget = mockRuleTarget("//src:lib")
      val externalTarget = mockRuleTarget("//external:guava")

      whenever(modService.isBzlmodEnabled).thenReturn(false)
      whenever(queryService.query("'//external:all-targets' + '//...:all-targets'"))
          .thenReturn(listOf(mainTarget, externalTarget))

      val client =
          BazelClient(
              useCquery = false,
              cqueryExpression = null,
              fineGrainedHashExternalRepos = emptySet(),
              excludeExternalTargets = false,
              excludeTargetsQuery = null)

      val targets = client.queryAllTargets()

      assertThat(targets.map { it.name }).containsOnly("//src:lib", "//external:guava")
      verify(queryService, never()).queryBzlmodRepos()
    }
  }

  @Test
  fun queryAllTargets_bzlmodEnabled_bazel9_cquery_includesBzlmodRepos() {
    runBlocking {
      val mainTarget = mockRuleTarget("//src:lib")
      val bzlmodTarget = mockRuleTarget("//external:rules_java~")

      whenever(modService.isBzlmodEnabled).thenReturn(true)
      whenever(queryService.canUseBzlmodShowRepo).thenReturn(true)
      whenever(queryService.query("deps(//...:all-targets)", useCquery = true))
          .thenReturn(listOf(mainTarget))
      whenever(queryService.queryBzlmodRepos()).thenReturn(listOf(bzlmodTarget))

      val client =
          BazelClient(
              useCquery = true,
              cqueryExpression = null,
              fineGrainedHashExternalRepos = emptySet(),
              excludeExternalTargets = false,
              excludeTargetsQuery = null)

      val targets = client.queryAllTargets()

      assertThat(targets).hasSize(2)
      assertThat(targets.map { it.name }).containsOnly("//src:lib", "//external:rules_java~")
      verify(queryService).queryBzlmodRepos()
    }
  }

  @Test
  fun queryAllTargets_bzlmodEnabled_bazel9_deduplicatesTargets() {
    runBlocking {
      val mainTarget = mockRuleTarget("//src:lib")
      val bzlmodTarget = mockRuleTarget("//src:lib") // duplicate name

      whenever(modService.isBzlmodEnabled).thenReturn(true)
      whenever(queryService.canUseBzlmodShowRepo).thenReturn(true)
      whenever(queryService.query("'//...:all-targets'")).thenReturn(listOf(mainTarget))
      whenever(queryService.queryBzlmodRepos()).thenReturn(listOf(bzlmodTarget))

      val client =
          BazelClient(
              useCquery = false,
              cqueryExpression = null,
              fineGrainedHashExternalRepos = emptySet(),
              excludeExternalTargets = false,
              excludeTargetsQuery = null)

      val targets = client.queryAllTargets()

      assertThat(targets).hasSize(1)
    }
  }

  @Test
  fun queryAllTargets_retriesWithoutExternal_whenExternalPackageUnavailable() {
    runBlocking {
      val mainTarget = mockRuleTarget("//src:lib")

      // Bzlmod probe false-negatives (reports disabled), so //external:all-targets is included, but
      // the combined query fails because //external is not actually available. The retry without it
      // succeeds.
      whenever(modService.isBzlmodEnabled).thenReturn(false)
      whenever(queryService.query("'//external:all-targets' + '//...:all-targets'"))
          .thenThrow(ExternalPackageUnavailableException("no such package 'external'"))
      whenever(queryService.query("'//...:all-targets'")).thenReturn(listOf(mainTarget))

      val client =
          BazelClient(
              useCquery = false,
              cqueryExpression = null,
              fineGrainedHashExternalRepos = emptySet(),
              excludeExternalTargets = false,
              excludeTargetsQuery = null)

      val targets = client.queryAllTargets()

      assertThat(targets.map { it.name }).containsOnly("//src:lib")
      verify(queryService).query("'//...:all-targets'")
    }
  }

  @Test
  fun queryAllTargets_cquery_dropsExternalRepoTargets_whenExternalPackageUnavailable() {
    runBlocking {
      val mainTarget = mockRuleTarget("//src:lib")

      whenever(modService.isBzlmodEnabled).thenReturn(false)
      whenever(queryService.query("deps(//...:all-targets)", useCquery = true))
          .thenReturn(listOf(mainTarget))
      whenever(queryService.query("'//external:all-targets'"))
          .thenThrow(ExternalPackageUnavailableException("no such package 'external'"))

      val client =
          BazelClient(
              useCquery = true,
              cqueryExpression = null,
              fineGrainedHashExternalRepos = emptySet(),
              excludeExternalTargets = false,
              excludeTargetsQuery = null)

      val targets = client.queryAllTargets()

      assertThat(targets.map { it.name }).containsOnly("//src:lib")
    }
  }

  @Test
  fun queryAllTargets_skipsExternalOnSubsequentCalls_afterUnavailable() {
    runBlocking {
      val mainTarget = mockRuleTarget("//src:lib")

      whenever(modService.isBzlmodEnabled).thenReturn(false)
      whenever(queryService.query("'//external:all-targets' + '//...:all-targets'"))
          .thenThrow(ExternalPackageUnavailableException("no such package 'external'"))
      whenever(queryService.query("'//...:all-targets'")).thenReturn(listOf(mainTarget))

      val client =
          BazelClient(
              useCquery = false,
              cqueryExpression = null,
              fineGrainedHashExternalRepos = emptySet(),
              excludeExternalTargets = false,
              excludeTargetsQuery = null)

      client.queryAllTargets() // learns //external is unavailable
      client.queryAllTargets() // should skip //external up front now

      // The combined (with-//external) query is attempted only on the first call; both calls fall
      // back to the //external-free query.
      verify(queryService, times(1)).query("'//external:all-targets' + '//...:all-targets'")
      verify(queryService, times(2)).query("'//...:all-targets'")
    }
  }

  @Test
  fun queryAllTargets_excludeTargetsQuery_wrapsNonCqueryQuery() {
    runBlocking {
      val mainTarget = mockRuleTarget("//src:lib")
      val excludeQuery = "attr(\"tags\", \"manual\", //...)"
      val wrapped = "('//external:all-targets' + '//...:all-targets') except ($excludeQuery)"

      whenever(modService.isBzlmodEnabled).thenReturn(false)
      whenever(queryService.query(wrapped)).thenReturn(listOf(mainTarget))

      val client =
          BazelClient(
              useCquery = false,
              cqueryExpression = null,
              fineGrainedHashExternalRepos = emptySet(),
              excludeExternalTargets = false,
              excludeTargetsQuery = excludeQuery)

      val targets = client.queryAllTargets()

      assertThat(targets.map { it.name }).containsOnly("//src:lib")
      verify(queryService).query(wrapped)
    }
  }

  @Test
  fun queryAllTargets_excludeTargetsQuery_wrapsCqueryExpression() {
    runBlocking {
      val mainTarget = mockRuleTarget("//src:lib")
      val excludeQuery = "attr(\"tags\", \"manual\", //...)"
      val wrapped = "(deps(//...:all-targets)) except ($excludeQuery)"

      whenever(modService.isBzlmodEnabled).thenReturn(true)
      whenever(queryService.canUseBzlmodShowRepo).thenReturn(false)
      whenever(queryService.query(wrapped, useCquery = true)).thenReturn(listOf(mainTarget))

      val client =
          BazelClient(
              useCquery = true,
              cqueryExpression = null,
              fineGrainedHashExternalRepos = emptySet(),
              excludeExternalTargets = false,
              excludeTargetsQuery = excludeQuery)

      val targets = client.queryAllTargets()

      assertThat(targets.map { it.name }).containsOnly("//src:lib")
      verify(queryService).query(wrapped, useCquery = true)
    }
  }

  @Test
  fun queryAllTargets_blankExcludeTargetsQuery_doesNotWrapQuery() {
    runBlocking {
      val mainTarget = mockRuleTarget("//src:lib")

      whenever(modService.isBzlmodEnabled).thenReturn(false)
      whenever(queryService.query("'//external:all-targets' + '//...:all-targets'"))
          .thenReturn(listOf(mainTarget))

      val client =
          BazelClient(
              useCquery = false,
              cqueryExpression = null,
              fineGrainedHashExternalRepos = emptySet(),
              excludeExternalTargets = false,
              excludeTargetsQuery = "   ")

      val targets = client.queryAllTargets()

      assertThat(targets.map { it.name }).containsOnly("//src:lib")
      verify(queryService).query("'//external:all-targets' + '//...:all-targets'")
    }
  }

  private fun mockRuleTarget(name: String): BazelTarget.Rule {
    val target = mock<BazelTarget.Rule>()
    whenever(target.name).thenReturn(name)
    return target
  }
}
