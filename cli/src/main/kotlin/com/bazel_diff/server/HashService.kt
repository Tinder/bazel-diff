package com.bazel_diff.server

import com.bazel_diff.bazel.BazelModService
import com.bazel_diff.hash.BuildGraphHasher
import com.bazel_diff.hash.TargetHash
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
  /** Returns hash data for [sha] (a fully-resolved commit SHA), generating + caching on miss. */
  fun getHashes(sha: String): HashFileData

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

  /** Cache key for [sha]: the SHA plus the config fingerprint, both filename-safe. */
  fun cacheKey(sha: String): String = "$sha.$configFingerprint"

  override fun getHashes(sha: String): HashFileData {
    storage.get(cacheKey(sha))?.let { bytes ->
      logger.i { "Hash cache hit for $sha" }
      return deserialiser.executeTargetHashWithMetadataFromString(
          String(bytes, StandardCharsets.UTF_8))
    }
    return generate(sha)
  }

  override fun <T> withWorkspaceAt(sha: String, block: () -> T): T =
      synchronized(generationLock) {
        gitClient.checkout(sha)
        block()
      }

  private fun generate(sha: String): HashFileData =
      synchronized(generationLock) {
        // Re-check under the lock: another thread may have generated this revision while we waited.
        storage.get(cacheKey(sha))?.let {
          logger.i { "Hash cache hit for $sha (after lock)" }
          return deserialiser.executeTargetHashWithMetadataFromString(
              String(it, StandardCharsets.UTF_8))
        }
        logger.i { "Hash cache miss for $sha - generating hashes" }
        gitClient.checkout(sha)
        val hashes =
            buildGraphHasher.hashAllBazelTargetsAndSourcefiles(
                seedFilepaths, ignoredRuleHashingAttributes)
        val moduleGraphJson = runBlocking { bazelModService.getModuleGraphJson() }
        val depEdges = depEdgesOf(hashes)
        storage.put(
            cacheKey(sha),
            serialize(hashes, moduleGraphJson, depEdges).toByteArray(StandardCharsets.UTF_8))
        HashFileData(hashes, moduleGraphJson, depEdges)
      }

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
