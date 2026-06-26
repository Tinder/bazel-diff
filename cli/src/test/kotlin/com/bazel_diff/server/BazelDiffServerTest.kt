package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.containsExactly
import assertk.assertions.isEqualTo
import com.bazel_diff.SilentLogger
import com.bazel_diff.log.Logger
import com.google.gson.Gson
import com.google.gson.GsonBuilder
import java.io.BufferedReader
import java.io.InputStreamReader
import java.net.HttpURLConnection
import java.net.URL
import java.nio.charset.StandardCharsets
import java.util.concurrent.atomic.AtomicBoolean
import org.junit.After
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.koin.dsl.module
import org.koin.test.KoinTest
import org.koin.test.KoinTestRule

class BazelDiffServerTest : KoinTest {
  @get:Rule
  val koinTestRule =
      KoinTestRule.create {
        modules(
            module {
              single<Logger> { SilentLogger }
              single { GsonBuilder().disableHtmlEscaping().create() }
            })
      }

  private val gson = Gson()
  private val ready = AtomicBoolean(true)
  private var provider: ImpactedTargetsProvider = FixedProvider()
  private lateinit var server: BazelDiffServer

  private class FixedProvider(
      var result: ImpactedTargetsResult =
          ImpactedTargetsResult("from-sha", "to-sha", listOf("//:a", "//:b")),
      var error: Exception? = null,
      var lastTargetTypes: Set<String>? = null,
  ) : ImpactedTargetsProvider {
    override fun getImpactedTargets(
        fromRev: String,
        toRev: String,
        targetTypes: Set<String>?
    ): ImpactedTargetsResult {
      lastTargetTypes = targetTypes
      error?.let { throw it }
      return result
    }
  }

  @Before
  fun setUp() {
    // Port 0 binds an ephemeral port so parallel test runs never collide.
    server = BazelDiffServer(0, providerProxy(), { ready.get() })
    server.start()
  }

  @After
  fun tearDown() {
    server.stop()
  }

  // Indirection so a test can swap `provider` after the server has been constructed.
  private fun providerProxy() =
      object : ImpactedTargetsProvider {
        override fun getImpactedTargets(fromRev: String, toRev: String, targetTypes: Set<String>?) =
            provider.getImpactedTargets(fromRev, toRev, targetTypes)
      }

  private data class Response(val code: Int, val body: String)

  private fun get(path: String): Response = request(path, "GET")

  private fun request(path: String, method: String): Response {
    val conn =
        URL("http://localhost:${server.boundPort()}$path").openConnection() as HttpURLConnection
    conn.requestMethod = method
    val code = conn.responseCode
    val stream = if (code in 200..299) conn.inputStream else conn.errorStream
    val body =
        stream?.let { BufferedReader(InputStreamReader(it, StandardCharsets.UTF_8)).readText() }
            ?: ""
    conn.disconnect()
    return Response(code, body)
  }

  @Test
  fun healthReturns200WhenReady() {
    ready.set(true)
    val response = get("/health")
    assertThat(response.code).isEqualTo(200)
    assertThat(response.body).contains("OK")
  }

  @Test
  fun healthReturns503WhenNotReady() {
    ready.set(false)
    assertThat(get("/health").code).isEqualTo(503)
  }

  @Test
  fun impactedTargetsReturnsJson() {
    provider = FixedProvider()
    val response = get("/impacted_targets?from=main&to=feature")
    assertThat(response.code).isEqualTo(200)
    val parsed = gson.fromJson(response.body, ImpactedTargetsResult::class.java)
    assertThat(parsed.from).isEqualTo("from-sha")
    assertThat(parsed.to).isEqualTo("to-sha")
    assertThat(parsed.impactedTargets).containsExactly("//:a", "//:b")
  }

  @Test
  fun impactedTargetsPassesTargetTypeFilter() {
    val fixed = FixedProvider()
    provider = fixed
    get("/impacted_targets?from=a&to=b&targetType=Rule,SourceFile")
    assertThat(fixed.lastTargetTypes).isEqualTo(setOf("Rule", "SourceFile"))
  }

  @Test
  fun impactedTargetsMissingParamsReturns400() {
    assertThat(get("/impacted_targets?from=main").code).isEqualTo(400)
    assertThat(get("/impacted_targets").code).isEqualTo(400)
  }

  @Test
  fun impactedTargetsReturns503WhenNotReady() {
    ready.set(false)
    assertThat(get("/impacted_targets?from=a&to=b").code).isEqualTo(503)
  }

  @Test
  fun gitErrorReturns400() {
    provider = FixedProvider(error = GitClientException("bad revision"))
    val response = get("/impacted_targets?from=a&to=b")
    assertThat(response.code).isEqualTo(400)
    assertThat(response.body).contains("git error")
  }

  @Test
  fun nonGetMethodReturns405() {
    assertThat(request("/impacted_targets?from=a&to=b", "POST").code).isEqualTo(405)
  }

  @Test
  fun valuelessQueryParamsAreIgnored() {
    // A bare `flag` param (no '=') must be skipped, not crash; from/to are still missing -> 400.
    assertThat(get("/impacted_targets?flag&from=a").code).isEqualTo(400)
  }

  @Test
  fun unexpectedErrorReturns500() {
    provider = FixedProvider(error = RuntimeException("boom"))
    val response = get("/impacted_targets?from=a&to=b")
    assertThat(response.code).isEqualTo(500)
    assertThat(response.body).contains("boom")
  }
}
