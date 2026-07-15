package com.bazel_diff.server

import com.bazel_diff.log.Logger
import com.google.gson.Gson
import com.google.gson.JsonSyntaxException
import com.sun.net.httpserver.HttpExchange
import com.sun.net.httpserver.HttpServer
import java.io.File
import java.io.IOException
import java.net.InetSocketAddress
import java.net.URLDecoder
import java.nio.charset.StandardCharsets
import java.nio.file.Path
import java.util.concurrent.Callable
import java.util.concurrent.ExecutionException
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.TimeoutException
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

/**
 * Minimal HTTP/JSON front-end for the bazel-diff query service (RFC: issue #29).
 *
 * Endpoints:
 * - `GET /health` -- `200 OK` once [readiness] reports ready, `503` otherwise. A load balancer uses
 *   this to route only to instances that have completed their initial git fetch and have not been
 *   lame-ducked by a fatal git error.
 * - `GET /impacted_targets?from=<rev>&to=<rev>[&targetType=Rule,SourceFile][&profile=true]` --
 *   returns `{"from": <sha>, "to": <sha>, "impactedTargets": [...]}`. With `profile=true` the
 *   response additionally carries `profile` (per-phase wall-clock breakdown, including per-side
 *   cache hit/miss -- see [QueryProfile]) and `memoryProfile` (JVM heap/GC movement -- see
 *   [MemoryProfile]) so callers can feed per-request metrics into their monitoring.
 * - `POST /impacted_targets` with a JSON body `{"from", "to", "targetType"?: [...],
 *   "modifiedFilepaths"?: [...], "profile"?: bool}` -- same result as the GET form, but
 *   additionally accepts `modifiedFilepaths` (workspace-relative paths changed between the
 *   revisions) to scope content hashing (see [ImpactedTargetsService]/[HashService]). A
 *   changed-file list is too large for a URL, so it rides in the body; a GET is always the
 *   full-content hash.
 * - `GET /impacted_targets_with_distances?from=<rev>&to=<rev>[&targetType=...]` (and its `POST`
 *   form) -- like above but each impacted target is `{"label", "targetDistance",
 *   "packageDistance"}`. Requires the server to have been started with `--trackDeps`; returns `400`
 *   otherwise.
 * - `GET /metrics` -- a JSON snapshot of the instance (version, uptime, readiness, git engine,
 *   cache size usage, JVM heap) when a [metricsProvider] is wired. Intentionally not gated on
 *   readiness, so a scrape of an un-ready or lame-ducked instance still returns data.
 *
 * Built on the JDK's [HttpServer] so the service needs no new third-party dependency. The handler
 * pool is unbounded (cached) so that health checks are always served even while a long hash
 * generation is in flight holding the workspace lock.
 */
class BazelDiffServer(
    private val port: Int,
    private val impactedTargetsProvider: ImpactedTargetsProvider,
    private val requestTimeoutSeconds: Long = 0,
    private val metricsProvider: MetricsProvider? = null,
    private val readiness: () -> Boolean,
) : KoinComponent {
  private val logger: Logger by inject()
  private val gson: Gson by inject()

  @Volatile private var server: HttpServer? = null

  // Runs the per-request query work when a timeout is configured, so the handler thread can abandon
  // a query that overruns [requestTimeoutSeconds]. Left idle (no threads created) when the timeout
  // is disabled, since the query then runs inline on the handler thread.
  private val computeExecutor: ExecutorService =
      Executors.newCachedThreadPool { runnable ->
        Thread(runnable, "bazel-diff-compute").apply { isDaemon = true }
      }

  /** Starts the server. Returns immediately; requests are served on background threads. */
  fun start() {
    val httpServer = HttpServer.create(InetSocketAddress(port), 0)
    // Daemon worker threads so a clean JVM shutdown (and the shutdown hook that calls stop()) is
    // never blocked by idle handler threads lingering in the pool.
    httpServer.executor =
        Executors.newCachedThreadPool { runnable ->
          Thread(runnable, "bazel-diff-server").apply { isDaemon = true }
        }
    httpServer.createContext("/health", ::handleHealth)
    httpServer.createContext("/impacted_targets", ::handleImpactedTargets)
    httpServer.createContext(
        "/impacted_targets_with_distances", ::handleImpactedTargetsWithDistances)
    httpServer.createContext("/metrics", ::handleMetrics)
    httpServer.start()
    server = httpServer
    logger.i { "bazel-diff query service listening on port ${boundPort()} " }
  }

  /** The actual bound port (useful when started with port 0 for an ephemeral port). */
  fun boundPort(): Int = server?.address?.port ?: port

  /** Stops the server, waiting up to [delaySeconds] for in-flight exchanges to finish. */
  fun stop(delaySeconds: Int = 0) {
    server?.stop(delaySeconds)
    server = null
    computeExecutor.shutdownNow()
  }

  // HttpExchange exposes close() but does not implement Closeable, so we close it explicitly.
  private inline fun withExchange(exchange: HttpExchange, block: () -> Unit) {
    try {
      block()
    } finally {
      exchange.close()
    }
  }

  private fun handleHealth(exchange: HttpExchange) =
      withExchange(exchange) {
        if (readiness()) {
          respond(exchange, 200, "OK\n")
        } else {
          respond(exchange, 503, "NOT_READY\n")
        }
      }

  private fun handleMetrics(exchange: HttpExchange) =
      withExchange(exchange) {
        if (!exchange.requestMethod.equals("GET", ignoreCase = true)) {
          respondJson(exchange, 405, mapOf("error" to "method not allowed, use GET"))
          return@withExchange
        }
        val provider = metricsProvider
        if (provider == null) {
          respondJson(exchange, 404, mapOf("error" to "metrics unavailable"))
          return@withExchange
        }
        // Deliberately not gated on readiness: metrics must be scrapeable even when un-ready.
        respondJson(exchange, 200, provider.snapshot())
      }

  private fun handleImpactedTargets(exchange: HttpExchange) =
      handleQuery(exchange) { from, to, targetTypes, modifiedFilepaths, profiler ->
        impactedTargetsProvider.getImpactedTargets(
            from, to, targetTypes, modifiedFilepaths, profiler)
      }

  private fun handleImpactedTargetsWithDistances(exchange: HttpExchange) =
      handleQuery(exchange) { from, to, targetTypes, modifiedFilepaths, profiler ->
        impactedTargetsProvider.getImpactedTargetsWithDistances(
            from, to, targetTypes, modifiedFilepaths, profiler)
      }

  /** Normalized inputs for a query, parsed from either a GET query string or a POST JSON body. */
  private data class QueryInputs(
      val from: String,
      val to: String,
      val targetTypes: Set<String>?,
      val modifiedFilepaths: Set<Path>,
      val profile: Boolean,
  )

  /**
   * JSON body accepted on `POST /impacted_targets(_with_distances)`. All fields optional here so a
   * missing one becomes a clear `400` rather than a Gson failure.
   */
  private data class ImpactedTargetsRequestBody(
      val from: String? = null,
      val to: String? = null,
      val targetType: List<String>? = null,
      val modifiedFilepaths: List<String>? = null,
      val profile: Boolean? = null,
  )

  /** Local signal that request parsing failed; [handleQuery] maps it to a `400`. */
  private class BadRequestException(message: String) : Exception(message)

  /**
   * Shared handling for the impacted-targets endpoints: enforces GET/POST + readiness, parses and
   * validates the request, then serializes the result of [compute] as JSON, mapping the known
   * failure modes to the appropriate status codes.
   */
  private fun handleQuery(
      exchange: HttpExchange,
      compute:
          (
              from: String,
              to: String,
              targetTypes: Set<String>?,
              modifiedFilepaths: Set<Path>,
              profiler: QueryProfiler?) -> Any
  ) =
      withExchange(exchange) {
        val isGet = exchange.requestMethod.equals("GET", ignoreCase = true)
        val isPost = exchange.requestMethod.equals("POST", ignoreCase = true)
        if (!isGet && !isPost) {
          respondJson(exchange, 405, mapOf("error" to "method not allowed, use GET or POST"))
          return@withExchange
        }
        if (!readiness()) {
          respondJson(exchange, 503, mapOf("error" to "service not ready"))
          return@withExchange
        }

        val inputs =
            try {
              if (isPost) parsePostBody(exchange) else parseGetQuery(exchange)
            } catch (e: BadRequestException) {
              respondJson(exchange, 400, mapOf("error" to (e.message ?: "bad request")))
              return@withExchange
            }

        // Created before dispatch so the profile's total also covers any wait for a compute slot /
        // the workspace lock -- the parts of latency a caller most wants visible.
        val profiler = if (inputs.profile) QueryProfiler() else null
        try {
          respondJson(
              exchange,
              200,
              computeWithTimeout {
                compute(
                    inputs.from, inputs.to, inputs.targetTypes, inputs.modifiedFilepaths, profiler)
              })
        } catch (e: TimeoutException) {
          logger.w { "request exceeded ${requestTimeoutSeconds}s timeout, abandoning" }
          respondJson(
              exchange, 504, mapOf("error" to "request timed out after ${requestTimeoutSeconds}s"))
        } catch (e: DistancesUnavailableException) {
          respondJson(exchange, 400, mapOf("error" to (e.message ?: "distances unavailable")))
        } catch (e: GitClientException) {
          logger.e(e) { "git error computing impacted targets" }
          respondJson(exchange, 400, mapOf("error" to "git error: ${e.message}"))
        } catch (e: Exception) {
          logger.e(e) { "error computing impacted targets" }
          respondJson(exchange, 500, mapOf("error" to (e.message ?: e.javaClass.simpleName)))
        }
      }

  /**
   * Runs [compute] bounded by [requestTimeoutSeconds]. When the budget is exceeded the in-flight
   * task is interrupted and a [TimeoutException] is thrown so the caller can respond `504`. Note
   * the underlying `bazel query` may not honor the interrupt and can keep running in the background
   * — abandoning it frees the client and the handler thread, and the query will still populate the
   * per-SHA cache so a retry is fast. A non-positive timeout (the default) runs [compute] inline on
   * the handler thread with no bound, preserving the original behavior.
   */
  private fun <T> computeWithTimeout(compute: () -> T): T {
    if (requestTimeoutSeconds <= 0) return compute()
    val future = computeExecutor.submit(Callable { compute() })
    try {
      return future.get(requestTimeoutSeconds, TimeUnit.SECONDS)
    } catch (e: TimeoutException) {
      future.cancel(true)
      throw e
    } catch (e: ExecutionException) {
      // Unwrap so the handler's per-type catch blocks (git error, distances unavailable, ...) see
      // the original exception rather than an ExecutionException wrapper.
      throw e.cause ?: e
    }
  }

  private fun respondJson(exchange: HttpExchange, status: Int, body: Any) {
    exchange.responseHeaders.add("Content-Type", "application/json")
    respond(exchange, status, gson.toJson(body))
  }

  private fun respond(exchange: HttpExchange, status: Int, body: String) {
    val bytes = body.toByteArray(StandardCharsets.UTF_8)
    try {
      exchange.sendResponseHeaders(status, bytes.size.toLong())
      exchange.responseBody.use { it.write(bytes) }
    } catch (e: IOException) {
      logger.w { "failed to write response: ${e.message}" }
    }
  }

  /**
   * Parses a GET request's `from`/`to`/`targetType`/`profile` from the query string. A GET never
   * carries modified filepaths (a changed-file list is too large for a URL), so it is always the
   * full-content hash. Throws [BadRequestException] when `from`/`to` are missing.
   */
  private fun parseGetQuery(exchange: HttpExchange): QueryInputs {
    val params = parseQuery(exchange.requestURI.rawQuery)
    val from = params["from"]
    val to = params["to"]
    if (from.isNullOrEmpty() || to.isNullOrEmpty()) {
      throw BadRequestException("missing required query parameters 'from' and 'to'")
    }
    val targetTypes =
        params["targetType"]?.split(",")?.map { it.trim() }?.filter { it.isNotEmpty() }?.toSet()
    return QueryInputs(from, to, targetTypes, emptySet(), profile = params["profile"].toBoolean())
  }

  /**
   * Parses a POST request's JSON body into [QueryInputs], including the optional
   * `modifiedFilepaths` scope (converted to workspace-relative [Path]s the same way
   * `--seed-filepaths` reads them). Throws [BadRequestException] for a malformed/empty body or a
   * missing `from`/`to`. An empty or all-blank `targetType` collapses to null (no filter, i.e. all
   * types).
   */
  private fun parsePostBody(exchange: HttpExchange): QueryInputs {
    val raw = exchange.requestBody.readBytes().toString(StandardCharsets.UTF_8)
    val body =
        try {
          gson.fromJson(raw, ImpactedTargetsRequestBody::class.java)
        } catch (e: JsonSyntaxException) {
          throw BadRequestException("invalid JSON body: ${e.message}")
        } ?: throw BadRequestException("missing JSON body with 'from' and 'to'")
    val from = body.from
    val to = body.to
    if (from.isNullOrEmpty() || to.isNullOrEmpty()) {
      throw BadRequestException("missing required fields 'from' and 'to'")
    }
    val targetTypes =
        body.targetType
            ?.map { it.trim() }
            ?.filter { it.isNotEmpty() }
            ?.toSet()
            ?.takeIf { it.isNotEmpty() }
    val modifiedFilepaths =
        body.modifiedFilepaths
            ?.map { it.trim() }
            ?.filter { it.isNotEmpty() }
            ?.map { File(it).toPath() }
            ?.toSet() ?: emptySet()
    return QueryInputs(from, to, targetTypes, modifiedFilepaths, profile = body.profile ?: false)
  }

  private fun parseQuery(rawQuery: String?): Map<String, String> {
    if (rawQuery.isNullOrEmpty()) return emptyMap()
    return rawQuery
        .split("&")
        .mapNotNull { pair ->
          val idx = pair.indexOf('=')
          if (idx < 0) null
          else {
            val key = decode(pair.substring(0, idx))
            val value = decode(pair.substring(idx + 1))
            key to value
          }
        }
        .toMap()
  }

  private fun decode(s: String): String = URLDecoder.decode(s, StandardCharsets.UTF_8.name())
}
