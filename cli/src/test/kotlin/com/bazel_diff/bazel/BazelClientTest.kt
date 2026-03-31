package com.bazel_diff.bazel

import assertk.assertThat
import assertk.assertions.containsOnly
import assertk.assertions.hasSize
import assertk.assertions.isEmpty
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
  fun queryAllTargets_bzlmodEnabled_bazel9_includesBzlmodRepos() { runBlocking {
    val mainTarget = mockRuleTarget("//src:lib")
    val bzlmodTarget = mockRuleTarget("//external:rules_java~")

    whenever(modService.isBzlmodEnabled).thenReturn(true)
    whenever(queryService.canUseBzlmodShowRepo).thenReturn(true)
    whenever(queryService.query("'//...:all-targets'")).thenReturn(listOf(mainTarget))
    whenever(queryService.queryBzlmodRepos()).thenReturn(listOf(bzlmodTarget))

    val client = BazelClient(
        useCquery = false,
        cqueryExpression = null,
        fineGrainedHashExternalRepos = emptySet(),
        excludeExternalTargets = false)

    val targets = client.queryAllTargets()

    assertThat(targets).hasSize(2)
    assertThat(targets.map { it.name }).containsOnly("//src:lib", "//external:rules_java~")
    verify(queryService).queryBzlmodRepos()
  } }

  @Test
  fun queryAllTargets_bzlmodEnabled_oldBazel_skipsBzlmodRepos() { runBlocking {
    val mainTarget = mockRuleTarget("//src:lib")

    whenever(modService.isBzlmodEnabled).thenReturn(true)
    whenever(queryService.canUseBzlmodShowRepo).thenReturn(false)
    whenever(queryService.query("'//...:all-targets'")).thenReturn(listOf(mainTarget))

    val client = BazelClient(
        useCquery = false,
        cqueryExpression = null,
        fineGrainedHashExternalRepos = emptySet(),
        excludeExternalTargets = false)

    val targets = client.queryAllTargets()

    assertThat(targets.map { it.name }).containsOnly("//src:lib")
    verify(queryService, never()).queryBzlmodRepos()
  } }

  @Test
  fun queryAllTargets_bzlmodDisabled_skipsBzlmodRepos() { runBlocking {
    val mainTarget = mockRuleTarget("//src:lib")
    val externalTarget = mockRuleTarget("//external:guava")

    whenever(modService.isBzlmodEnabled).thenReturn(false)
    whenever(queryService.query("'//external:all-targets' + '//...:all-targets'"))
        .thenReturn(listOf(mainTarget, externalTarget))

    val client = BazelClient(
        useCquery = false,
        cqueryExpression = null,
        fineGrainedHashExternalRepos = emptySet(),
        excludeExternalTargets = false)

    val targets = client.queryAllTargets()

    assertThat(targets.map { it.name }).containsOnly("//src:lib", "//external:guava")
    verify(queryService, never()).queryBzlmodRepos()
  } }

  @Test
  fun queryAllTargets_bzlmodEnabled_bazel9_cquery_includesBzlmodRepos() { runBlocking {
    val mainTarget = mockRuleTarget("//src:lib")
    val bzlmodTarget = mockRuleTarget("//external:rules_java~")

    whenever(modService.isBzlmodEnabled).thenReturn(true)
    whenever(queryService.canUseBzlmodShowRepo).thenReturn(true)
    whenever(queryService.query("deps(//...:all-targets)", useCquery = true))
        .thenReturn(listOf(mainTarget))
    whenever(queryService.queryBzlmodRepos()).thenReturn(listOf(bzlmodTarget))

    val client = BazelClient(
        useCquery = true,
        cqueryExpression = null,
        fineGrainedHashExternalRepos = emptySet(),
        excludeExternalTargets = false)

    val targets = client.queryAllTargets()

    assertThat(targets).hasSize(2)
    assertThat(targets.map { it.name }).containsOnly("//src:lib", "//external:rules_java~")
    verify(queryService).queryBzlmodRepos()
  } }

  @Test
  fun queryAllTargets_bzlmodEnabled_bazel9_deduplicatesTargets() { runBlocking {
    val mainTarget = mockRuleTarget("//src:lib")
    val bzlmodTarget = mockRuleTarget("//src:lib") // duplicate name

    whenever(modService.isBzlmodEnabled).thenReturn(true)
    whenever(queryService.canUseBzlmodShowRepo).thenReturn(true)
    whenever(queryService.query("'//...:all-targets'")).thenReturn(listOf(mainTarget))
    whenever(queryService.queryBzlmodRepos()).thenReturn(listOf(bzlmodTarget))

    val client = BazelClient(
        useCquery = false,
        cqueryExpression = null,
        fineGrainedHashExternalRepos = emptySet(),
        excludeExternalTargets = false)

    val targets = client.queryAllTargets()

    assertThat(targets).hasSize(1)
  } }

  private fun mockRuleTarget(name: String): BazelTarget.Rule {
    val target = mock<BazelTarget.Rule>()
    whenever(target.name).thenReturn(name)
    return target
  }
}
