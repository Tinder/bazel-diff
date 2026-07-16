package com.bazel_diff.server

import com.bazel_diff.bazel.BazelModService
import com.bazel_diff.extensions.toHexString
import com.bazel_diff.hash.BuildGraphHasher
import com.bazel_diff.hash.HasherPhaseTimings
import com.bazel_diff.hash.TargetHash
import com.bazel_diff.hash.sha256
import com.bazel_diff.interactor.DeserialiseHashesInteractor
import com.bazel_diff.interactor.HashFileData
import com.bazel_diff.log.Logger
import com.google.gson.Gson
import java.nio.charset.StandardCharsets
import java.nio.file.Path
import kotlinx.coroutines.runBlocking
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

/**
 * Produces [HashFileData] for a fully-resolved commit SHA, caching the generated hashes so repeated
 * requests for the same revision skip the expensive `bazel query`.
 *
 * Split behind an interface so [ImpactedTargetsService] can be unit-tested with a fake.
 */
interface HashProvider {
  /**
   * Returns hash data for [sha] (a fully-resolved commit SHA), generating + caching on miss.
   *
   * [modifiedFilepaths], when non-empty, scopes source-file *content* hashing to just those
   * workspace-relative paths (see [BuildGraphHasher.hashAllBazelTargetsAndSourcefiles]) and is
   * folded into the cache key, so a content-scoped entry is never served to -- or mixed with -- a
   * request using a different set. Empty (the default) is the full-content hash and today's per-SHA
   * cache key. Correctness requires the caller to apply the *same* set to both revisions of a
   * comparison and for it to be a superset of what actually changed (see [ImpactedTargetsService]).
   *
   * [profiler], when non-null, receives a [HashRetrievalProfile] for this call (cache hit/miss and
   * duration).
   */
  fun getHashes(
      sha: String,
      modifiedFilepaths: Set<Path> = emptySet(),
      profiler: QueryProfiler? = null,
  ): HashFileData

  /**
   * Runs [block] with the workspace checked out at [sha], holding the workspace lock for the
   * duration. Used by the module-change path, which issues a live Bazel query against the working
   * tree and therefore needs it pinned to a known revision while it runs.
   */
  fun <T> withWorkspaceAt(sha: String, block: () -> T): T
}

/**
 * [HashProvider] backed by [BuildGraphHasher] + a [HashCacheStorage]. All workspace-mutating work
 * (git checkout, `bazel query`) is serialized by [generationLock] because every request shares one
 * workspace clone and one Bazel server.
 *
 * @param configFingerprint short digest of the query-affecting flags, mixed into the cache key so a
 *   server started with different flags never reads another configuration's cached hashes.
 * @param seedFilepaths seed files passed through to [BuildGraphHasher].
 * @param ignoredRuleHashingAttributes rule attributes ignored when hashing.
 * @param trackDeps when true, persist each revision's dependency-edge adjacency list in the cache
 *   so build-graph distance metrics can be served. Requires the hasher to have been built with
 *   dep-tracking on (so [TargetHash.deps] is populated).
 */
class HashService(
    private val gitClient: GitClient,
    private val storage: HashCacheStorage,
    private val configFingerprint: String,
    private val seedFilepaths: Set<Path>,
    private val ignoredRuleHashingAttributes: Set<String>,
    private val trackDeps: Boolean = false,
) : HashProvider, KoinComponent {
  private val buildGraphHasher: BuildGraphHasher by inject()
  private val bazelModService: BazelModService by inject()
  private val gson: Gson by inject()
  private val logger: Logger by inject()
  private val deserialiser = DeserialiseHashesInteractor()

  // Guards every workspace-mutating operation: only one checkout + query may run at a time.
  private val generationLock = Any()

  /**
   * Cache key for [sha]: the SHA plus the config fingerprint, both filename-safe. A non-empty
   * [modifiedFilepaths] appends a short digest of the (sorted) set so a content-scoped hash is only
   * ever compared against another hash scoped by the same set -- never the full-content entry, nor
   * a differently-scoped one. Empty (the common path) keeps the original `<sha>.<fingerprint>` key
   * so GET requests and warmup keep sharing entries. The digest is order-independent (paths are
   * sorted) and hex, so the on-disk `<key>.json` mapping stays filename-safe.
   */
  fun cacheKey(sha: String, modifiedFilepaths: Set<Path> = emptySet()): String {
    val base = "$sha.$configFingerprint"
    if (modifiedFilepaths.isEmpty()) return base
    val digest =
        sha256 {
              for (path in modifiedFilepaths.map { it.toString() }.sorted()) {
                putBytes(path.toByteArray())
                putBytes(byteArrayOf(0x0a))
              }
            }
            .toHexString()
            .take(12)
    return "$base.$digest"
  }

  /**
   * Outcome of one [retrieve]: the data, whether it was a cache hit, and where the time went --
   * everything [getHashes] needs to assemble a [HashRetrievalProfile] except the total duration.
   */
  private data class Retrieval(
      val data: HashFileData,
      val cacheHit: Boolean,
      val lockWaitMillis: Long? = null,
      val cacheReadMillis: Long? = null,
      val generation: HashGenerationBreakdown? = null,
  )

  override fun getHashes(
      sha: String,
      modifiedFilepaths: Set<Path>,
      profiler: QueryProfiler?
  ): HashFileData {
    val startNanos = System.nanoTime()
    val retrieval = retrieve(sha, modifiedFilepaths)
    profiler?.recordHashRetrieval(
        HashRetrievalProfile(
            sha = sha,
            cacheHit = retrieval.cacheHit,
            durationMillis = (System.nanoTime() - startNanos) / 1_000_000,
            lockWaitMillis = retrieval.lockWaitMillis,
            cacheReadMillis = retrieval.cacheReadMillis,
            generation = retrieval.generation))
    return retrieval.data
  }

  /**
   * Returns the hash data plus whether it was served from the cache. "Hit" includes the
   * waited-behind-another-generation case (the after-lock re-check): this request itself ran no
   * checkout/query, though its duration then includes the lock wait.
   */
  private fun retrieve(sha: String, modifiedFilepaths: Set<Path>): Retrieval {
    val key = cacheKey(sha, modifiedFilepaths)
    val readStartNanos = System.nanoTime()
    storage.get(key)?.let { bytes ->
      val data =
          deserialiser.executeTargetHashWithMetadataFromString(
              String(bytes, StandardCharsets.UTF_8))
      val readMillis = elapsedMillis(readStartNanos)
      logger.i { "Hash cache hit for $sha (read+deserialize ${readMillis}ms)" }
      return Retrieval(data, cacheHit = true, cacheReadMillis = readMillis)
    }
    return generate(sha, modifiedFilepaths, key)
  }

  override fun <T> withWorkspaceAt(sha: String, block: () -> T): T =
      synchronized(generationLock) {
        gitClient.checkout(sha)
        block()
      }

  private fun generate(sha: String, modifiedFilepaths: Set<Path>, key: String): Retrieval {
    val lockStartNanos = System.nanoTime()
    synchronized(generationLock) {
      val lockWaitMillis = elapsedMillis(lockStartNanos)
      // Re-check under the lock: another thread may have generated this revision while we waited.
      val readStartNanos = System.nanoTime()
      storage.get(key)?.let {
        val data =
            deserialiser.executeTargetHashWithMetadataFromString(String(it, StandardCharsets.UTF_8))
        val readMillis = elapsedMillis(readStartNanos)
        logger.i {
          "Hash cache hit for $sha (after ${lockWaitMillis}ms lock wait, " +
              "read+deserialize ${readMillis}ms)"
        }
        return Retrieval(
            data, cacheHit = true, lockWaitMillis = lockWaitMillis, cacheReadMillis = readMillis)
      }
      logger.i { "Hash cache miss for $sha - generating hashes" }

      val checkoutStartNanos = System.nanoTime()
      gitClient.checkout(sha)
      val checkoutMillis = elapsedMillis(checkoutStartNanos)

      val hasherTimings = HasherPhaseTimings()
      val hashes =
          buildGraphHasher.hashAllBazelTargetsAndSourcefiles(
              seedFilepaths, ignoredRuleHashingAttributes, modifiedFilepaths, hasherTimings)

      val moduleGraphStartNanos = System.nanoTime()
      val moduleGraphJson = runBlocking { bazelModService.getModuleGraphJson() }
      val moduleGraphMillis = elapsedMillis(moduleGraphStartNanos)

      val writeStartNanos = System.nanoTime()
      val depEdges = depEdgesOf(hashes)
      storage.put(
          key, serialize(hashes, moduleGraphJson, depEdges).toByteArray(StandardCharsets.UTF_8))
      val cacheWriteMillis = elapsedMillis(writeStartNanos)

      val breakdown =
          HashGenerationBreakdown(
              checkoutMillis = checkoutMillis,
              bazelQueryMillis = hasherTimings.bazelQueryMillis,
              sourceHashMillis = hasherTimings.sourceHashMillis,
              targetHashMillis = hasherTimings.targetHashMillis,
              moduleGraphMillis = moduleGraphMillis,
              cacheWriteMillis = cacheWriteMillis,
              targetCount = hashes.size,
          )
      // Always logged (not just when the request opted into profile=true) so slow generations are
      // attributable to a phase straight from the server logs.
      logger.i {
        "Hash generation for $sha took ${elapsedMillis(lockStartNanos)}ms: " +
            "lockWait=${lockWaitMillis}ms checkout=${checkoutMillis}ms " +
            "bazelQuery=${hasherTimings.bazelQueryMillis}ms " +
            "sourceHash=${hasherTimings.sourceHashMillis}ms " +
            "targetHash=${hasherTimings.targetHashMillis}ms " +
            "moduleGraph=${moduleGraphMillis}ms cacheWrite=${cacheWriteMillis}ms " +
            "targets=${hashes.size}"
      }
      return Retrieval(
          HashFileData(hashes, moduleGraphJson, depEdges),
          cacheHit = false,
          lockWaitMillis = lockWaitMillis,
          generation = breakdown)
    }
  }

  private fun elapsedMillis(startNanos: Long): Long = (System.nanoTime() - startNanos) / 1_000_000

  /**
   * The dependency-edge adjacency list (label -> direct dep labels) when [trackDeps] is on, else
   * empty. Derived from [TargetHash.deps], the same way `generate-hashes --depEdgesFile` derives
   * it.
   */
  private fun depEdgesOf(hashes: Map<String, TargetHash>): Map<String, List<String>> =
      if (trackDeps) hashes.mapValues { it.value.deps ?: emptyList() } else emptyMap()

  /**
   * Serializes hashes into the same JSON shape `generate-hashes` writes (see
   * [com.bazel_diff.interactor.GenerateHashesInteractor]), so cached entries are interchangeable
   * with hashes produced by the CLI. Target type is always included for the richest data. When
   * [depEdges] is non-empty (i.e. --trackDeps), it is persisted under `metadata.depEdges` so
   * distance metrics can be served on a cache hit.
   */
  private fun serialize(
      hashes: Map<String, TargetHash>,
      moduleGraphJson: String?,
      depEdges: Map<String, List<String>>
  ): String {
    val serializedHashes = hashes.mapValues { it.value.toJson(true) }
    val output =
        if (moduleGraphJson != null || depEdges.isNotEmpty()) {
          val metadata = mutableMapOf<String, Any>()
          if (moduleGraphJson != null) metadata["moduleGraphJson"] = moduleGraphJson
          if (depEdges.isNotEmpty()) metadata["depEdges"] = depEdges
          mapOf("hashes" to serializedHashes, "metadata" to metadata)
        } else {
          serializedHashes
        }
    return gson.toJson(output)
  }
}
