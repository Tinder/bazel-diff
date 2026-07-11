package com.bazel_diff.server

import assertk.assertThat
import assertk.assertions.contains
import assertk.assertions.containsExactly
import assertk.assertions.isEqualTo
import assertk.assertions.isNull
import com.bazel_diff.SilentLogger
import com.bazel_diff.interactor.ImpactedTargetWithDistance
import com.bazel_diff.log.Logger
import com.google.gson.Gson
import com.google.gson.GsonBuilder
import java.io.BufferedReader
import java.io.InputStreamReader
import java.net.HttpURLConnection
import java.net.URL
import java.nio.charset.StandardCharsets
import java.nio.file.Path
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
  private val metrics: MetricsProvider = FixedMetrics()
  private lateinit var server: BazelDiffServer

  private class FixedMetrics : MetricsProvider {
    override fun snapshot() =
        ServerMetrics(
            version = "9.9.9",
            uptimeSeconds = 42,
            ready = true,
            gitEngine = "subprocess",
            trackDeps = false,
            cache = CacheMetrics("/cache", 2, 2048, "2.0 KB"),
            jvm = JvmMetrics(1000, 2000),
        )
  }

  private class FixedProvider(
      var result: ImpactedTargetsResult =
          ImpactedTargetsResult("from-sha", "to-sha", listOf("//:a", "//:b")),
      var distancesResult: ImpactedTargetsWithDistancesResult =
          ImpactedTargetsWithDistancesResult(
              "from-sha",
              "to-sha",
              listOf(
                  ImpactedTargetWithDistance("//:a", 0, 0),
                  ImpactedTargetWithDistance("//:b", 1, 1))),
      var error: Exception? = null,
      var distancesError: Exception? = null,
      var lastFrom: String? = null,
      var lastTo: String? = null,
      var lastTargetTypes: Set<String>? = null,
      var lastModifiedFilepaths: Set<Path> = emptySet(),
  ) : ImpactedTargetsProvider {
    override fun getImpactedTargets(
        fromRev: String,
        toRev: String,
        targetTypes: Set<String>?,
        modifiedFilepaths: Set<Path>,
    ): ImpactedTargetsResult {
      record(fromRev, toRev, targetTypes, modifiedFilepaths)
      error?.let { throw it }
      return result
    }

    override fun getImpactedTargetsWithDistances(
        fromRev: String,
        toRev: String,
        targetTypes: Set<String>?,
        modifiedFilepaths: Set<Path>,
    ): ImpactedTargetsWithDistancesResult {
      record(fromRev, toRev, targetTypes, modifiedFilepaths)
      distancesError?.let { throw it }
      return distancesResult
    }

    private fun record(
        fromRev: String,
        toRev: String,
        targetTypes: Set<String>?,
        modifiedFilepaths: Set<Path>
    ) {
      lastFrom = fromRev
      lastTo = toRev
      lastTargetTypes = targetTypes
      lastModifiedFilepaths = modifiedFilepaths
    }
  }

  @Before
  fun setUp() {
    // Port 0 binds an ephemeral port so parallel test runs never collide.
    server = BazelDiffServer(0, providerProxy(), metricsProvider = metrics) { ready.get() }
    server.start()
  }

  @After
  fun tearDown() {
    server.stop()
  }

  // Indirection so a test can swap `provider` after the server has been constructed.
  private fun providerProxy() =
      object : ImpactedTargetsProvider {
        override fun getImpactedTargets(
            fromRev: String,
            toRev: String,
            targetTypes: Set<String>?,
            modifiedFilepaths: Set<Path>
        ) = provider.getImpactedTargets(fromRev, toRev, targetTypes, modifiedFilepaths)

        override fun getImpactedTargetsWithDistances(
            fromRev: String,
            toRev: String,
            targetTypes: Set<String>?,
            modifiedFilepaths: Set<Path>
        ) = provider.getImpactedTargetsWithDistances(fromRev, toRev, targetTypes, modifiedFilepaths)
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

  private fun post(path: String, body: String): Response {
    val conn =
        URL("http://localhost:${server.boundPort()}$path").openConnection() as HttpURLConnection
    conn.requestMethod = "POST"
    conn.doOutput = true
    conn.setRequestProperty("Content-Type", "application/json")
    conn.outputStream.use { it.write(body.toByteArray(StandardCharsets.UTF_8)) }
    val code = conn.responseCode
    val stream = if (code in 200..299) conn.inputStream else conn.errorStream
    val respBody =
        stream?.let { BufferedReader(InputStreamReader(it, StandardCharsets.UTF_8)).readText() }
            ?: ""
    conn.disconnect()
    return Response(code, respBody)
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
  fun unsupportedMethodReturns405() {
    // GET and POST are both valid now; anything else is 405.
    assertThat(request("/impacted_targets?from=a&to=b", "PUT").code).isEqualTo(405)
    assertThat(request("/impacted_targets?from=a&to=b", "DELETE").code).isEqualTo(405)
  }

  @Test
  fun postScopedRequestParsesBodyAndPassesModifiedFilepaths() {
    val fixed = FixedProvider()
    provider = fixed
    val body =
        """{"from":"main","to":"feature","targetType":["Rule"],""" +
            """"modifiedFilepaths":["pkg/A.kt","pkg/B.kt"]}"""
    val response = post("/impacted_targets", body)

    assertThat(response.code).isEqualTo(200)
    val parsed = gson.fromJson(response.body, ImpactedTargetsResult::class.java)
    assertThat(parsed.impactedTargets).containsExactly("//:a", "//:b")
    assertThat(fixed.lastFrom).isEqualTo("main")
    assertThat(fixed.lastTo).isEqualTo("feature")
    assertThat(fixed.lastTargetTypes).isEqualTo(setOf("Rule"))
    assertThat(fixed.lastModifiedFilepaths)
        .isEqualTo(setOf(Path.of("pkg/A.kt"), Path.of("pkg/B.kt")))
  }

  @Test
  fun postWithoutModifiedFilepathsIsAFullHash() {
    val fixed = FixedProvider()
    provider = fixed
    val response = post("/impacted_targets", """{"from":"a","to":"b"}""")

    assertThat(response.code).isEqualTo(200)
    // Absent modifiedFilepaths/targetType behave exactly like the GET path: full hash, no filter.
    assertThat(fixed.lastModifiedFilepaths).isEqualTo(emptySet())
    assertThat(fixed.lastTargetTypes).isNull()
  }

  @Test
  fun postMalformedBodyReturns400() {
    assertThat(post("/impacted_targets", "{not json").code).isEqualTo(400)
  }

  @Test
  fun postMissingFromOrEmptyBodyReturns400() {
    assertThat(post("/impacted_targets", """{"to":"b"}""").code).isEqualTo(400)
    assertThat(post("/impacted_targets", "").code).isEqualTo(400)
  }

  @Test
  fun postScopedDistancesRequestPassesModifiedFilepaths() {
    val fixed = FixedProvider()
    provider = fixed
    val body = """{"from":"main","to":"feature","modifiedFilepaths":["pkg/A.kt"]}"""
    val response = post("/impacted_targets_with_distances", body)

    assertThat(response.code).isEqualTo(200)
    assertThat(fixed.lastModifiedFilepaths).isEqualTo(setOf(Path.of("pkg/A.kt")))
  }

  @Test
  fun postReturns503WhenNotReady() {
    ready.set(false)
    assertThat(post("/impacted_targets", """{"from":"a","to":"b"}""").code).isEqualTo(503)
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

  @Test
  fun impactedTargetsWithDistancesReturnsJson() {
    provider = FixedProvider()
    val response = get("/impacted_targets_with_distances?from=main&to=feature")
    assertThat(response.code).isEqualTo(200)
    val parsed = gson.fromJson(response.body, ImpactedTargetsWithDistancesResult::class.java)
    assertThat(parsed.from).isEqualTo("from-sha")
    assertThat(parsed.to).isEqualTo("to-sha")
    assertThat(parsed.impactedTargets)
        .containsExactly(
            ImpactedTargetWithDistance("//:a", 0, 0), ImpactedTargetWithDistance("//:b", 1, 1))
  }

  @Test
  fun impactedTargetsWithDistancesPassesTargetTypeFilter() {
    val fixed = FixedProvider()
    provider = fixed
    get("/impacted_targets_with_distances?from=a&to=b&targetType=Rule")
    assertThat(fixed.lastTargetTypes).isEqualTo(setOf("Rule"))
  }

  @Test
  fun impactedTargetsWithDistancesMissingParamsReturns400() {
    assertThat(get("/impacted_targets_with_distances?from=main").code).isEqualTo(400)
  }

  @Test
  fun impactedTargetsWithDistancesReturns503WhenNotReady() {
    ready.set(false)
    assertThat(get("/impacted_targets_with_distances?from=a&to=b").code).isEqualTo(503)
  }

  @Test
  fun slowRequestTimesOutWith504() {
    // A provider that blocks longer than the server's 1s request budget must be abandoned with 504
    // rather than making the client wait for the full compute.
    val slowProvider =
        object : ImpactedTargetsProvider {
          override fun getImpactedTargets(
              fromRev: String,
              toRev: String,
              targetTypes: Set<String>?,
              modifiedFilepaths: Set<Path>
          ): ImpactedTargetsResult {
            Thread.sleep(10_000)
            return ImpactedTargetsResult(fromRev, toRev, emptyList())
          }

          override fun getImpactedTargetsWithDistances(
              fromRev: String,
              toRev: String,
              targetTypes: Set<String>?,
              modifiedFilepaths: Set<Path>
          ) = throw UnsupportedOperationException()
        }
    val slowServer = BazelDiffServer(0, slowProvider, requestTimeoutSeconds = 1) { true }
    slowServer.start()
    try {
      val conn =
          URL("http://localhost:${slowServer.boundPort()}/impacted_targets?from=a&to=b")
              .openConnection() as HttpURLConnection
      val code = conn.responseCode
      val body =
          (conn.errorStream ?: conn.inputStream)?.let {
            BufferedReader(InputStreamReader(it, StandardCharsets.UTF_8)).readText()
          } ?: ""
      conn.disconnect()
      assertThat(code).isEqualTo(504)
      assertThat(body).contains("timed out")
    } finally {
      slowServer.stop()
    }
  }

  @Test
  fun distancesUnavailableReturns400() {
    provider =
        FixedProvider(
            distancesError =
                DistancesUnavailableException(
                    "distances unavailable: server started without --trackDeps"))
    val response = get("/impacted_targets_with_distances?from=a&to=b")
    assertThat(response.code).isEqualTo(400)
    assertThat(response.body).contains("--trackDeps")
  }

  @Test
  fun metricsReturnsSnapshotJson() {
    val response = get("/metrics")
    assertThat(response.code).isEqualTo(200)
    val parsed = gson.fromJson(response.body, ServerMetrics::class.java)
    assertThat(parsed.version).isEqualTo("9.9.9")
    assertThat(parsed.cache.entries).isEqualTo(2L)
    assertThat(parsed.jvm.maxBytes).isEqualTo(2000L)
  }

  @Test
  fun metricsIsServedEvenWhenNotReady() {
    // Metrics must be scrapeable on an un-ready/lame-ducked instance, so it is not gated on
    // readiness.
    ready.set(false)
    assertThat(get("/metrics").code).isEqualTo(200)
  }

  @Test
  fun metricsRejectsNonGetWith405() {
    assertThat(request("/metrics", "POST").code).isEqualTo(405)
  }

  @Test
  fun metricsReturns404WhenNoProviderWired() {
    val bare = BazelDiffServer(0, providerProxy()) { ready.get() }
    bare.start()
    try {
      val conn =
          URL("http://localhost:${bare.boundPort()}/metrics").openConnection() as HttpURLConnection
      assertThat(conn.responseCode).isEqualTo(404)
      conn.disconnect()
    } finally {
      bare.stop()
    }
  }
}
