package com.bazel_diff.server

import com.bazel_diff.log.Logger
import com.google.gson.Gson
import com.sun.net.httpserver.HttpExchange
import com.sun.net.httpserver.HttpServer
import java.io.IOException
import java.net.InetSocketAddress
import java.net.URLDecoder
import java.nio.charset.StandardCharsets
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
 * - `GET /impacted_targets?from=<rev>&to=<rev>[&targetType=Rule,SourceFile]` -- returns `{"from":
 *   <sha>, "to": <sha>, "impactedTargets": [...]}`.
 * - `GET /impacted_targets_with_distances?from=<rev>&to=<rev>[&targetType=...]` -- like above but
 *   each impacted target is `{"label", "targetDistance", "packageDistance"}`. Requires the server
 *   to have been started with `--trackDeps`; returns `400` otherwise.
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
      handleQuery(exchange) { from, to, targetTypes ->
        impactedTargetsProvider.getImpactedTargets(from, to, targetTypes)
      }

  private fun handleImpactedTargetsWithDistances(exchange: HttpExchange) =
      handleQuery(exchange) { from, to, targetTypes ->
        impactedTargetsProvider.getImpactedTargetsWithDistances(from, to, targetTypes)
      }

  /**
   * Shared handling for the impacted-targets endpoints: enforces GET + readiness, parses and
   * validates `from`/`to`/`targetType`, then serializes the result of [compute] as JSON, mapping
   * the known failure modes to the appropriate status codes.
   */
  private fun handleQuery(
      exchange: HttpExchange,
      compute: (from: String, to: String, targetTypes: Set<String>?) -> Any
  ) =
      withExchange(exchange) {
        if (!exchange.requestMethod.equals("GET", ignoreCase = true)) {
          respondJson(exchange, 405, mapOf("error" to "method not allowed, use GET"))
          return@withExchange
        }
        if (!readiness()) {
          respondJson(exchange, 503, mapOf("error" to "service not ready"))
          return@withExchange
        }

        val params = parseQuery(exchange.requestURI.rawQuery)
        val from = params["from"]
        val to = params["to"]
        if (from.isNullOrEmpty() || to.isNullOrEmpty()) {
          respondJson(
              exchange, 400, mapOf("error" to "missing required query parameters 'from' and 'to'"))
          return@withExchange
        }
        val targetTypes =
            params["targetType"]?.split(",")?.map { it.trim() }?.filter { it.isNotEmpty() }?.toSet()

        try {
          respondJson(exchange, 200, computeWithTimeout { compute(from, to, targetTypes) })
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
