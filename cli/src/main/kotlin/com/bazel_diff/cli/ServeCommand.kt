package com.bazel_diff.cli

import com.bazel_diff.cli.converter.ByteSizeConverter
import com.bazel_diff.cli.converter.CommaSeparatedValueConverter
import com.bazel_diff.cli.converter.DurationConverter
import com.bazel_diff.cli.converter.NormalisingPathConverter
import com.bazel_diff.cli.converter.OptionsConverter
import com.bazel_diff.di.hasherModule
import com.bazel_diff.di.loggingModule
import com.bazel_diff.di.serialisationModule
import com.bazel_diff.extensions.toHexString
import com.bazel_diff.hash.sha256
import com.bazel_diff.server.BazelDiffServer
import com.bazel_diff.server.CachePruneLimits
import com.bazel_diff.server.CachePruner
import com.bazel_diff.server.GitClient
import com.bazel_diff.server.HashCacheStorage
import com.bazel_diff.server.HashProvider
import com.bazel_diff.server.HashService
import com.bazel_diff.server.ImpactedTargetsService
import com.bazel_diff.server.LocalDiskHashCacheStorage
import com.bazel_diff.server.MetricsService
import com.bazel_diff.server.ProcessGitClient
import com.bazel_diff.server.PrunableHashCacheStorage
import java.io.File
import java.nio.file.Path
import java.time.Duration
import java.util.concurrent.Callable
import java.util.concurrent.CountDownLatch
import java.util.concurrent.atomic.AtomicBoolean
import org.koin.core.context.startKoin
import org.koin.core.context.stopKoin
import picocli.CommandLine

/**
 * Long-running HTTP/JSON query service that answers affectedness queries between two git revisions
 * (RFC: issue #29).
 *
 * The service is scoped to a single workspace clone at [workspacePath]. On startup it performs an
 * initial `git fetch` and only then reports healthy. For each request it resolves the `from`/`to`
 * revisions, generates (and caches, keyed by SHA) the hashes for each, and reuses the same
 * affectedness logic as `get-impacted-targets`.
 *
 * Query-affecting flags mirror `generate-hashes` so the hashes the service produces match what a
 * cold CLI run would produce.
 */
@CommandLine.Command(
    name = "serve",
    mixinStandardHelpOptions = true,
    description =
        [
            "Runs bazel-diff as a long-running HTTP query service that returns the impacted targets " +
                "between two git revisions, caching generated hashes per commit SHA."],
    versionProvider = VersionProvider::class)
class ServeCommand : Callable<Int> {
  @CommandLine.ParentCommand private lateinit var parent: BazelDiff

  @CommandLine.Option(
      names = ["-w", "--workspacePath"],
      description = ["Path to the Bazel workspace git clone the service checks out and queries."],
      required = true,
      converter = [NormalisingPathConverter::class])
  lateinit var workspacePath: Path

  @CommandLine.Option(
      names = ["-b", "--bazelPath"],
      description =
          [
              "Path to Bazel binary. If not specified, the Bazel binary available in PATH will be used."],
      defaultValue = "bazel")
  lateinit var bazelPath: Path

  @CommandLine.Option(
      names = ["--gitPath"],
      description =
          [
              "Path to the git binary used for fetch/checkout operations. Defaults to 'git' on the PATH."],
      defaultValue = "git")
  var gitPath: String = "git"

  @CommandLine.Option(
      names = ["--port"],
      description = ["Port to listen on. Defaults to 8080."],
      defaultValue = "8080")
  var port: Int = 8080

  @CommandLine.Option(
      names = ["--requestTimeout"],
      description =
          [
              "Maximum seconds an /impacted_targets(_with_distances) request may run before the " +
                  "server abandons it and responds 504. 0 (the default) means no timeout. This " +
                  "bounds the request the client waits on; an in-flight bazel query may keep " +
                  "running in the background and still populate the per-SHA cache."],
      defaultValue = "0")
  var requestTimeoutSeconds: Long = 0

  @CommandLine.Option(
      names = ["--cacheDir"],
      description =
          ["Directory where generated hashes are cached per commit SHA. Persists across restarts."],
      required = true,
      converter = [NormalisingPathConverter::class])
  lateinit var cacheDir: Path

  @CommandLine.Option(
      names = ["--cacheMaxAge"],
      description =
          [
              "Evict cached hashes not read or written within this window, so the cache does not " +
                  "grow without bound over time. Duration like 7d, 36h, 90m (units d/h/m/s). Unset " +
                  "means no age limit. Enforced by a background sweeper (see --cachePruneInterval)."],
      converter = [DurationConverter::class])
  var cacheMaxAge: Duration? = null

  @CommandLine.Option(
      names = ["--cacheMaxEntries"],
      description =
          [
              "Keep at most this many cached commit-SHA entries, evicting the least-recently-used " +
                  "first. Unset means no count limit."])
  var cacheMaxEntries: Int? = null

  @CommandLine.Option(
      names = ["--cacheMaxSize"],
      description =
          [
              "Keep the cache's total on-disk size at or below this, evicting the " +
                  "least-recently-used entries first. Size like 10GB, 500MB, or a bare byte count " +
                  "(base 1024). Unset means no size limit."],
      converter = [ByteSizeConverter::class])
  var cacheMaxSize: Long? = null

  @CommandLine.Option(
      names = ["--cachePruneInterval"],
      description =
          [
              "How often the background sweeper enforces the --cacheMax* limits. Duration like 1h, " +
                  "30m. Defaults to 1h. No effect unless a --cacheMax* limit is set."],
      converter = [DurationConverter::class],
      defaultValue = "1h")
  var cachePruneInterval: Duration = Duration.ofHours(1)

  @CommandLine.Option(
      names = ["--warmupRevision"],
      description =
          [
              "Comma separated git revisions (branch/tag/SHA) whose hashes are generated and cached " +
                  "at startup, before the server reports healthy, so the first real request is warm " +
                  "and the Bazel analysis server is primed. Best-effort: a revision that fails to " +
                  "warm is logged and the server still becomes ready (serving it cold on demand). " +
                  "Increases time-to-healthy, so size deploy/health-check timeouts accordingly."],
      converter = [CommaSeparatedValueConverter::class])
  var warmupRevisions: Set<String> = emptySet()

  @CommandLine.Option(
      names = ["--no-initial-fetch"],
      description =
          [
              "Skip the initial 'git fetch' before reporting healthy. Useful for local/offline testing."])
  var noInitialFetch = false

  @CommandLine.Option(
      names = ["-so", "--bazelStartupOptions"],
      description =
          ["Additional space separated Bazel client startup options used when invoking Bazel"],
      converter = [OptionsConverter::class])
  var bazelStartupOptions: List<String> = emptyList()

  @CommandLine.Option(
      names = ["-co", "--bazelCommandOptions"],
      description =
          ["Additional space separated Bazel command options used when invoking `bazel query`"],
      converter = [OptionsConverter::class])
  var bazelCommandOptions: List<String> = emptyList()

  @CommandLine.Option(
      names = ["--cqueryCommandOptions"],
      description =
          [
              "Additional space separated Bazel command options used when invoking `bazel cquery`. No effect unless --useCquery is set."],
      converter = [OptionsConverter::class])
  var cqueryCommandOptions: List<String> = emptyList()

  @CommandLine.Option(
      names = ["--useCquery"],
      negatable = true,
      description = ["If true, use cquery instead of query when generating dependency graphs."])
  var useCquery = false

  @CommandLine.Option(
      names = ["--cqueryExpression"],
      description =
          ["Custom cquery expression to use instead of the default. No effect unless --useCquery."])
  var cqueryExpression: String? = null

  @CommandLine.Option(
      names = ["-k", "--keep_going"],
      negatable = true,
      defaultValue = "false",
      fallbackValue = "true",
      description = ["Run `bazel query` with --keep_going. Defaults to false."])
  var keepGoing = false

  @CommandLine.Option(
      names = ["--fineGrainedHashExternalRepos"],
      description =
          ["Comma separated list of external repos for which fine-grained hashes are computed."],
      converter = [CommaSeparatedValueConverter::class])
  var fineGrainedHashExternalRepos: Set<String> = emptySet()

  @CommandLine.Option(
      names = ["--fineGrainedHashExternalReposFile"],
      description =
          [
              "A text file with a newline separated list of external repos. Mutually exclusive with --fineGrainedHashExternalRepos."])
  var fineGrainedHashExternalReposFile: File? = null

  @CommandLine.Option(
      names = ["--ignoredRuleHashingAttributes"],
      description = ["Attributes that should be ignored when hashing rule targets."],
      converter = [CommaSeparatedValueConverter::class])
  var ignoredRuleHashingAttributes: Set<String> = emptySet()

  @CommandLine.Option(
      names = ["-s", "--seed-filepaths"],
      description =
          [
              "A text file with a newline separated list of filepaths used as a SHA256 seed for all targets."])
  var seedFilepaths: File? = null

  @CommandLine.Option(
      names = ["--excludeExternalTargets"],
      negatable = true,
      description = ["If true, exclude external targets (do not query //external:all-targets)."])
  var excludeExternalTargets = false

  @CommandLine.Option(
      names = ["--excludeTargetsQuery"],
      description =
          [
              "A Bazel query expression whose matched targets are excluded from the served hashes " +
                  "via the `except` operator, e.g. `manual`-tagged targets: " +
                  "--excludeTargetsQuery='attr(\"tags\", \"[\\[ ]manual[,\\]]\", //...)'."])
  var excludeTargetsQuery: String? = null

  @CommandLine.Option(
      names = ["--trackDeps"],
      negatable = true,
      description =
          [
              "Track dependency edges and persist them per commit SHA so build-graph distance " +
                  "metrics can be served via /impacted_targets_with_distances. Increases cache " +
                  "size and memory. Defaults to false."])
  var trackDeps = false

  // The background cache sweeper, when a --cacheMax* limit is configured. Held so the shutdown path
  // can stop it; null when pruning is disabled or the backend manages its own retention.
  private var cachePruner: CachePruner? = null

  override fun call(): Int {
    org.koin.core.context.GlobalContext.stopKoin()
    startKoin {
      modules(
          hasherModule(
              workspacePath,
              bazelPath,
              null,
              bazelStartupOptions,
              bazelCommandOptions,
              cqueryCommandOptions,
              useCquery,
              cqueryExpression,
              keepGoing,
              trackDeps,
              fineGrainedHashExternalRepos,
              fineGrainedHashExternalReposFile,
              excludeExternalTargets,
              excludeTargetsQuery,
          ),
          loggingModule(parent.verbose),
          serialisationModule(),
      )
    }

    return try {
      val server = buildAndStartServer(createGitClient(), LocalDiskHashCacheStorage(cacheDir))
      awaitShutdown(server)
      CommandLine.ExitCode.OK
    } finally {
      stopKoin()
    }
  }

  /**
   * Builds the [GitClient]. Git fetch/checkout operations shell out to the `git` binary at
   * [gitPath], so a `git` binary must be available on the host.
   */
  fun createGitClient(): GitClient = ProcessGitClient(workspacePath, gitPath)

  /**
   * Wires the services, starts the HTTP server, and performs the initial git fetch + readiness
   * handshake. Returns the started [BazelDiffServer]. Requires only a logging/serialisation Koin
   * context to be active (the bazel-backed singletons are injected lazily and not touched until an
   * `/impacted_targets` request arrives), which keeps this independently unit-testable.
   */
  fun buildAndStartServer(gitClient: GitClient, storage: HashCacheStorage): BazelDiffServer {
    val hashService =
        HashService(
            gitClient,
            storage,
            computeConfigFingerprint(),
            loadSeedFilepaths(),
            ignoredRuleHashingAttributes,
            trackDeps)
    val impactedTargetsService =
        ImpactedTargetsService(gitClient, hashService, depsTracked = trackDeps)

    val ready = AtomicBoolean(false)
    val metrics =
        MetricsService(
            version = VersionProvider().version.firstOrNull() ?: "unknown",
            startedAtMillis = System.currentTimeMillis(),
            gitEngine = "subprocess",
            trackDeps = trackDeps,
            cacheDir = cacheDir.toString(),
            storage = storage,
            readiness = { ready.get() },
        )
    val server =
        BazelDiffServer(port, impactedTargetsService, requestTimeoutSeconds, metrics) {
          ready.get()
        }
    server.start()
    performInitialFetch(gitClient, hashService, ready, server)
    // Start sweeping only after warmup, so the immediate first pass evaluates an already-warm
    // cache.
    cachePruner = buildCachePruner(storage)?.also { it.start() }
    return server
  }

  /**
   * The cache-pruning limits requested via the `--cacheMax*` flags (all-null = pruning disabled).
   */
  fun cachePruneLimits(): CachePruneLimits =
      CachePruneLimits(maxAge = cacheMaxAge, maxEntries = cacheMaxEntries, maxBytes = cacheMaxSize)

  /**
   * Builds the background [CachePruner] for [storage], or null when no `--cacheMax*` limit is set
   * or the backend manages its own retention. Only a [PrunableHashCacheStorage] can be swept
   * in-process -- a remote backend (e.g. S3) would instead rely on a bucket lifecycle policy, so a
   * limit set against a non-prunable backend is reported and ignored rather than silently
   * pretended-to.
   */
  fun buildCachePruner(storage: HashCacheStorage): CachePruner? {
    val limits = cachePruneLimits()
    if (!limits.hasAny) return null
    if (storage !is PrunableHashCacheStorage) {
      System.err.println(
          "[Warn] --cacheMax* is set but the cache backend does not support in-process pruning; " +
              "ignoring (rely on the backend's own retention policy instead)")
      return null
    }
    return CachePruner(storage, limits, cachePruneInterval)
  }

  /**
   * Performs the startup git fetch (unless [noInitialFetch]) and flips [ready] to true once the
   * clone is known good. On a fetch failure the server is left running but un-ready ("lame duck")
   * so health checks report `503` and a load balancer removes the instance rather than us
   * attempting a risky in-place repair (RFC issue #29).
   */
  fun performInitialFetch(
      gitClient: GitClient,
      hashProvider: HashProvider,
      ready: AtomicBoolean,
      server: BazelDiffServer
  ) {
    if (noInitialFetch) {
      warmUpCache(gitClient, hashProvider)
      ready.set(true)
      return
    }
    try {
      gitClient.fetch()
      // Warm up before flipping ready so the load balancer only routes to this instance once its
      // configured baseline revisions are cached and the Bazel server is primed.
      warmUpCache(gitClient, hashProvider)
      ready.set(true)
      System.err.println("[Info] initial git fetch complete; serving on port ${server.boundPort()}")
    } catch (e: Exception) {
      System.err.println(
          "[Error] initial git fetch failed; server is lame-ducked (health will report NOT_READY): ${e.message}")
    }
  }

  /**
   * Best-effort priming of the hash cache (and, as a side effect, the Bazel analysis server) for
   * each `--warmupRevision` before readiness is reported. A revision that fails to warm (bad ref,
   * transient bazel error) is logged and skipped rather than failing the deploy: warmup is an
   * optimization, not a correctness requirement, so the server still becomes ready and serves that
   * revision cold on demand. Never throws, so a bad `--warmupRevision` cannot lame-duck the
   * instance.
   */
  fun warmUpCache(gitClient: GitClient, hashProvider: HashProvider) {
    for (rev in warmupRevisions) {
      try {
        val sha = gitClient.resolveSha(rev)
        System.err.println("[Info] warming hash cache for revision '$rev' ($sha)")
        hashProvider.getHashes(sha)
        System.err.println("[Info] warmed hash cache for revision '$rev' ($sha)")
      } catch (e: Exception) {
        System.err.println(
            "[Warn] warmup for revision '$rev' failed; server will serve it cold on demand: ${e.message}")
      }
    }
  }

  /** Reads the optional seed filepaths file into a set of paths. */
  fun loadSeedFilepaths(): Set<Path> =
      seedFilepaths?.readLines()?.filter { it.isNotBlank() }?.map { File(it).toPath() }?.toSet()
          ?: emptySet()

  /** Blocks until a JVM shutdown signal (or thread interrupt), stopping the server cleanly. */
  private fun awaitShutdown(server: BazelDiffServer) {
    val latch = CountDownLatch(1)
    Runtime.getRuntime()
        .addShutdownHook(
            Thread {
              cachePruner?.stop()
              server.stop(1)
              latch.countDown()
            })
    try {
      latch.await()
    } catch (e: InterruptedException) {
      // Treated as a shutdown signal: stop serving and restore the interrupt flag.
      cachePruner?.stop()
      server.stop(1)
      Thread.currentThread().interrupt()
    }
  }

  /**
   * Short digest over the query-affecting flags, mixed into every cache key so hashes generated
   * under one configuration are never served to a server started with a different one. The
   * workspace SHA already determines file contents (including seed/ignored-attr file contents at
   * that commit), so only the flag values themselves need to be captured here.
   */
  fun computeConfigFingerprint(): String {
    val canonical = buildString {
      append("useCquery=").append(useCquery).append('\n')
      append("cqueryExpression=").append(cqueryExpression ?: "").append('\n')
      append("keepGoing=").append(keepGoing).append('\n')
      append("excludeExternalTargets=").append(excludeExternalTargets).append('\n')
      append("excludeTargetsQuery=").append(excludeTargetsQuery ?: "").append('\n')
      append("bazelCommandOptions=").append(bazelCommandOptions.joinToString(" ")).append('\n')
      append("cqueryCommandOptions=").append(cqueryCommandOptions.joinToString(" ")).append('\n')
      append("bazelStartupOptions=").append(bazelStartupOptions.joinToString(" ")).append('\n')
      append("fineGrainedHashExternalRepos=")
          .append(fineGrainedHashExternalRepos.sorted().joinToString(","))
          .append('\n')
      append("fineGrainedHashExternalReposFile=")
          .append(fineGrainedHashExternalReposFile?.path ?: "")
          .append('\n')
      append("ignoredRuleHashingAttributes=")
          .append(ignoredRuleHashingAttributes.sorted().joinToString(","))
          .append('\n')
      // Distinct cache keys for deps-tracked vs deps-less runs: a deps-tracking server must never
      // serve a deps-less cache entry (its distance queries would have no edges to traverse).
      append("trackDeps=").append(trackDeps).append('\n')
      append("version=").append(VersionProvider().version.firstOrNull() ?: "unknown").append('\n')
    }
    return sha256 { putBytes(canonical.toByteArray()) }.toHexString().take(12)
  }
}
