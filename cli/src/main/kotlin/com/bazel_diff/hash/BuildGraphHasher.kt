package com.bazel_diff.hash

import com.bazel_diff.bazel.BazelClient
import com.bazel_diff.bazel.BazelModService
import com.bazel_diff.bazel.BazelRule
import com.bazel_diff.bazel.BazelSourceFileTarget
import com.bazel_diff.bazel.BazelTarget
import com.bazel_diff.extensions.toHexString
import com.bazel_diff.log.Logger
import com.google.common.collect.Sets
import java.nio.file.Path
import java.util.Calendar
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.ConcurrentMap
import java.util.concurrent.atomic.AtomicReference
import java.util.stream.Collectors
import kotlin.io.path.readBytes
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

class BuildGraphHasher(private val bazelClient: BazelClient) : KoinComponent {
  private val targetHasher: TargetHasher by inject()
  private val sourceFileHasher: SourceFileHasher by inject()
  private val bazelModService: BazelModService by inject()
  private val logger: Logger by inject()

  fun hashAllBazelTargetsAndSourcefiles(
      seedFilepaths: Set<Path> = emptySet(),
      ignoredAttrs: Set<String> = emptySet(),
      modifiedFilepaths: Set<Path> = emptySet()
  ): Map<String, TargetHash> {
    val (sourceDigests, allTargets) =
        runBlocking {
          val targetsTask = async(Dispatchers.IO) { bazelClient.queryAllTargets() }
          val allTargets = targetsTask.await()
          val sourceTargets =
              allTargets
                  .filter { it is BazelTarget.SourceFile }
                  .map { it as BazelTarget.SourceFile }

          val sourceDigestsFuture =
              async(Dispatchers.IO) {
                val sourceHashDurationEpoch = Calendar.getInstance().getTimeInMillis()
                val sourceFileTargets = hashSourcefiles(sourceTargets, modifiedFilepaths)
                val sourceHashDuration =
                    Calendar.getInstance().getTimeInMillis() - sourceHashDurationEpoch
                logger.i { "Source file hashes calculated in $sourceHashDuration" }
                sourceFileTargets
              }

          Pair(sourceDigestsFuture.await(), allTargets)
        }
    val seedForFilepaths =
        runBlocking(Dispatchers.IO) { createSeedForFilepaths(seedFilepaths) }
    // Attribute each BUILD file's loaded `.bzl` digests to the package that loads them, so a
    // `.bzl` edit only re-hashes targets in packages that actually `load()` it -- not every
    // target in the workspace (issue #365). A package that loads no tracked `.bzl` gets nothing
    // mixed in, keeping its targets' hashes byte-for-byte stable.
    val packageBzlSeeds = createPackageBzlSeeds(allTargets, modifiedFilepaths)
    return hashAllTargets(
        seedForFilepaths, packageBzlSeeds, sourceDigests, allTargets, ignoredAttrs, modifiedFilepaths)
  }

  private fun hashSourcefiles(
      targets: List<BazelTarget.SourceFile>,
      modifiedFilepaths: Set<Path>
  ): ConcurrentMap<String, ByteArray> {
    val exception = AtomicReference<Exception?>(null)
    val result: ConcurrentMap<String, ByteArray> =
        targets
            .parallelStream()
            .map { sourceFile: BazelTarget.SourceFile ->
              val seed = sha256 {
                safePutBytes(sourceFile.name.toByteArray())
                for (subinclude in sourceFile.subincludeList) {
                  safePutBytes(subinclude.toByteArray())
                }
              }
              try {
                val sourceFileTarget = BazelSourceFileTarget(sourceFile.name, seed)
                Pair(
                    sourceFileTarget.name,
                    sourceFileHasher.digest(sourceFileTarget, modifiedFilepaths))
              } catch (e: Exception) {
                exception.set(e)
                null
              }
            }
            .filter { pair -> pair != null }
            .collect(
                Collectors.toConcurrentMap(
                    { pair -> pair!!.first },
                    { pair -> pair!!.second },
                ))

    exception.get()?.let { throw it }
    return result
  }

  private fun hashAllTargets(
      seedHash: ByteArray,
      packageBzlSeeds: Map<String, ByteArray>,
      sourceDigests: ConcurrentMap<String, ByteArray>,
      allTargets: List<BazelTarget>,
      ignoredAttrs: Set<String>,
      modifiedFilepaths: Set<Path>
  ): Map<String, TargetHash> {
    val ruleHashes: ConcurrentMap<String, TargetDigest> = ConcurrentHashMap()
    val targetToRule: MutableMap<String, BazelRule> = HashMap()
    traverseGraph(allTargets, targetToRule)

    return allTargets
        .parallelStream()
        .map { target: BazelTarget ->
          val targetDigest =
              targetHasher.digest(
                  target,
                  targetToRule,
                  sourceDigests,
                  ruleHashes,
                  seedHash,
                  packageBzlSeeds,
                  ignoredAttrs,
                  modifiedFilepaths)
          Pair(
              target.name,
              TargetHash(
                  target.javaClass.name.substringAfterLast('$'),
                  targetDigest.overallDigest.toHexString(),
                  targetDigest.directDigest.toHexString(),
                  targetDigest.deps,
              ))
        }
        .filter { targetEntry: Pair<String, TargetHash>? -> targetEntry != null }
        .collect(
            Collectors.toMap(
                { obj: Pair<String, TargetHash> -> obj.first },
                { obj: Pair<String, TargetHash> -> obj.second },
            ))
  }

  /** Traverses the list of targets and revisits the targets with yet-unknown generating rule */
  private fun traverseGraph(
      allTargets: List<BazelTarget>,
      targetToRule: MutableMap<String, BazelRule>
  ) {
    var targetsToAnalyse: Set<BazelTarget> = Sets.newHashSet(allTargets)
    while (!targetsToAnalyse.isEmpty()) {
      val initialSize = targetsToAnalyse.size
      val nextTargets: MutableSet<BazelTarget> = Sets.newHashSet()
      for (target in targetsToAnalyse) {
        val targetName = target.name
        when (target) {
          is BazelTarget.GeneratedFile -> {
            targetToRule[target.generatingRuleName]?.let { targetToRule[targetName] = it }
                ?: nextTargets.add(target)
          }
          is BazelTarget.Rule -> targetToRule[targetName] = target.rule
          is BazelTarget.SourceFile -> continue
        }
      }
      val newSize = nextTargets.size
      if (newSize >= initialSize) {
        throw RuntimeException("Not possible to traverse the build graph")
      }
      targetsToAnalyse = nextTargets
    }
  }

  private suspend fun createSeedForFilepaths(seedFilepaths: Set<Path>): ByteArray {
    return sha256 {
      // Include seed filepaths in hash
      for (path in seedFilepaths) {
        putBytes(path.readBytes())
      }
    }
  }

  /**
   * Builds a per-package seed-hash contribution from the contents of every `.bzl` (and `.scl`)
   * file that package's BUILD file loads, keyed by package label (e.g. `//pkg`, `//` for the
   * root package).
   *
   * Background: Bazel pre-7 populated [Build.Rule.skylark_environment_hash_code] in the query
   * proto, so any change to a `.bzl` file loaded by a rule's BUILD file naturally bubbled into
   * that rule's hash. Bazel 7+ leaves that field empty, which is the root cause of issues
   * [#259](https://github.com/Tinder/bazel-diff/issues/259) and
   * [#227](https://github.com/Tinder/bazel-diff/issues/227): editing a macro body (e.g. adding
   * `print()`) no longer invalidated any caller because the emitted rule attrs were identical.
   *
   * Fix: each package's BUILD `SourceFile` target carries a `subincludeList` (the
   * `Build.SourceFile.subinclude` proto field) listing every `.bzl` that BUILD loaded. We
   * softDigest each main-repo `.bzl` and roll the digests for a given package into a seed
   * attributed to that package. Callers then mix only their own package's seed into each
   * target's hash, so editing a `.bzl` re-hashes exactly the targets in packages that
   * `load()` it -- not every target in the workspace
   * ([#365](https://github.com/Tinder/bazel-diff/issues/365)). A package that loads no tracked
   * `.bzl` has no entry here, so nothing is mixed in and its targets stay byte-for-byte stable.
   *
   * External-repo `.bzl` files (`@repo//...`, `@@canonical//...`) are skipped because
   * [SourceFileHasher.softDigest] returns null for non-main-repo labels, which keeps the seed
   * stable across BCR fetches that don't actually change repo contents.
   */
  private fun createPackageBzlSeeds(
      allTargets: List<BazelTarget>,
      modifiedFilepaths: Set<Path>
  ): Map<String, ByteArray> {
    // Union the loaded `.bzl` labels per package. Every source file in a package shares the same
    // BUILD, but the `subinclude` field is populated on the BUILD's SourceFile target; group by
    // package so we don't depend on which source file carries it.
    val packageToLabels = sortedMapOf<String, java.util.SortedSet<String>>()
    for (target in allTargets) {
      if (target !is BazelTarget.SourceFile || target.subincludeList.isEmpty()) continue
      packageToLabels
          .getOrPut(labelToPackage(target.sourceFileName)) { sortedSetOf() }
          .addAll(target.subincludeList)
    }

    val result = mutableMapOf<String, ByteArray>()
    for ((pkg, labels) in packageToLabels) {
      val tracked = mutableListOf<Pair<String, ByteArray>>()
      for (label in labels) {
        val digest =
            sourceFileHasher.softDigest(
                BazelSourceFileTarget(label, ByteArray(0)), modifiedFilepaths)
        if (digest != null) tracked += label to digest
      }
      if (tracked.isEmpty()) continue
      result[pkg] = sha256 {
        for ((label, digest) in tracked) {
          safePutBytes(label.toByteArray())
          safePutBytes(digest)
        }
      }
    }
    return result
  }
}

/**
 * Returns the package portion of a Bazel label, i.e. everything before the target separator `:`.
 * `//pkg:a` -> `//pkg`, `//:logo` -> `//`, `@@repo//pkg:a` -> `@@repo//pkg`. Labels without a `:`
 * (not expected from `bazel query` output) are returned unchanged.
 */
internal fun labelToPackage(label: String): String {
  val colon = label.lastIndexOf(':')
  return if (colon >= 0) label.substring(0, colon) else label
}
