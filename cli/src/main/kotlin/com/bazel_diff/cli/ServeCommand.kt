package com.bazel_diff.cli

import com.bazel_diff.cli.converter.CommaSeparatedValueConverter
import com.bazel_diff.cli.converter.NormalisingPathConverter
import com.bazel_diff.cli.converter.OptionsConverter
import com.bazel_diff.di.hasherModule
import com.bazel_diff.di.loggingModule
import com.bazel_diff.di.serialisationModule
import com.bazel_diff.extensions.toHexString
import com.bazel_diff.hash.sha256
import com.bazel_diff.server.BazelDiffServer
import com.bazel_diff.server.GitClient
import com.bazel_diff.server.HashCacheStorage
import com.bazel_diff.server.HashService
import com.bazel_diff.server.ImpactedTargetsService
import com.bazel_diff.server.JGitClient
import com.bazel_diff.server.LocalDiskHashCacheStorage
import com.bazel_diff.server.ProcessGitClient
import java.io.File
import java.nio.file.Path
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

  @CommandLine.Spec lateinit var spec: CommandLine.Model.CommandSpec

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
              "Path to the git binary, used only when --gitEngine=subprocess. Defaults to 'git' on the PATH."],
      defaultValue = "git")
  var gitPath: String = "git"

  @CommandLine.Option(
      names = ["--gitEngine"],
      description =
          [
              "Git backend: 'jgit' (in-process, no git binary required) or 'subprocess' (shells out to --gitPath). Defaults to 'jgit'."],
      defaultValue = "jgit")
  var gitEngine: String = "jgit"

  @CommandLine.Option(
      names = ["--port"],
      description = ["Port to listen on. Defaults to 8080."],
      defaultValue = "8080")
  var port: Int = 8080

  @CommandLine.Option(
      names = ["--cacheDir"],
      description =
          ["Directory where generated hashes are cached per commit SHA. Persists across restarts."],
      required = true,
      converter = [NormalisingPathConverter::class])
  lateinit var cacheDir: Path

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
      defaultValue = "true",
      fallbackValue = "true",
      description = ["Run `bazel query` with --keep_going. Defaults to true."])
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
      names = ["--trackDeps"],
      negatable = true,
      description =
          [
              "Track dependency edges and persist them per commit SHA so build-graph distance " +
                  "metrics can be served via /impacted_targets_with_distances. Increases cache " +
                  "size and memory. Defaults to false."])
  var trackDeps = false

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
   * Builds the [GitClient] for the configured [gitEngine]. Defaults to the in-process JGit engine;
   * `subprocess` falls back to shelling out to [gitPath] (matching C-git behavior exactly, useful
   * for workspaces relying on checkout filters/hooks JGit does not run).
   */
  fun createGitClient(): GitClient =
      when (gitEngine.lowercase()) {
        "jgit" -> JGitClient(workspacePath)
        "subprocess" -> ProcessGitClient(workspacePath, gitPath)
        else ->
            throw CommandLine.ParameterException(
                spec.commandLine(),
                "Unknown --gitEngine '$gitEngine' (expected 'jgit' or 'subprocess')")
      }

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
    val server = BazelDiffServer(port, impactedTargetsService) { ready.get() }
    server.start()
    performInitialFetch(gitClient, ready, server)
    return server
  }

  /**
   * Performs the startup git fetch (unless [noInitialFetch]) and flips [ready] to true once the
   * clone is known good. On a fetch failure the server is left running but un-ready ("lame duck")
   * so health checks report `503` and a load balancer removes the instance rather than us
   * attempting a risky in-place repair (RFC issue #29).
   */
  fun performInitialFetch(gitClient: GitClient, ready: AtomicBoolean, server: BazelDiffServer) {
    if (noInitialFetch) {
      ready.set(true)
      return
    }
    try {
      gitClient.fetch()
      ready.set(true)
      System.err.println("[Info] initial git fetch complete; serving on port ${server.boundPort()}")
    } catch (e: Exception) {
      System.err.println(
          "[Error] initial git fetch failed; server is lame-ducked (health will report NOT_READY): ${e.message}")
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
              server.stop(1)
              latch.countDown()
            })
    try {
      latch.await()
    } catch (e: InterruptedException) {
      // Treated as a shutdown signal: stop serving and restore the interrupt flag.
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
