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
    val seedForBzlFiles = createSeedForBzlFiles(allTargets, modifiedFilepaths)
    // Only mix the bzl-files seed when at least one main-repo `.bzl` was actually hashed.
    // This keeps the seed (and therefore every target's hash) byte-for-byte stable for
    // workspaces that don't load any tracked .bzl files, preserving cached hashes from
    // before this change.
    val combinedSeed =
        if (seedForBzlFiles != null) {
          sha256 {
            safePutBytes(seedForFilepaths)
            safePutBytes(seedForBzlFiles)
          }
        } else {
          seedForFilepaths
        }
    return hashAllTargets(
        combinedSeed, sourceDigests, allTargets, ignoredAttrs, modifiedFilepaths)
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
   * Builds a seed-hash contribution from the contents of every `.bzl` (and `.scl`) file the
   * workspace's BUILD files load.
   *
   * Background: Bazel pre-7 populated [Build.Rule.skylark_environment_hash_code] in the query
   * proto, so any change to a `.bzl` file loaded by a rule's BUILD file naturally bubbled into
   * that rule's hash. Bazel 7+ leaves that field empty, which is the root cause of issues
   * [#259](https://github.com/Tinder/bazel-diff/issues/259) and
   * [#227](https://github.com/Tinder/bazel-diff/issues/227): editing a macro body (e.g. adding
   * `print()`) no longer invalidated any caller because the emitted rule attrs were identical.
   *
   * Fix: walk every queried SourceFile target's `subincludeList` (the `Build.SourceFile.subinclude`
   * proto field, which already lists every `.bzl` the BUILD file loaded), softDigest each main-repo
   * `.bzl`, and roll the union of digests into the workspace-wide seed. This is intentionally
   * conservative -- a single `.bzl` edit re-hashes every target -- because `.bzl` edits are rare
   * and missing them silently is the worse failure mode. Per-package precision would require
   * mapping each rule to its package's BUILD file, which isn't a direct dep relationship in the
   * query proto.
   *
   * External-repo `.bzl` files (`@repo//...`, `@@canonical//...`) are skipped because
   * [SourceFileHasher.softDigest] returns null for non-main-repo labels, which keeps the seed
   * stable across BCR fetches that don't actually change repo contents.
   */
  private fun createSeedForBzlFiles(
      allTargets: List<BazelTarget>,
      modifiedFilepaths: Set<Path>
  ): ByteArray? {
    val subincludes =
        allTargets
            .asSequence()
            .filterIsInstance<BazelTarget.SourceFile>()
            .flatMap { it.subincludeList.asSequence() }
            .toSortedSet()
    val tracked = mutableListOf<Pair<String, ByteArray>>()
    for (label in subincludes) {
      val digest =
          sourceFileHasher.softDigest(
              BazelSourceFileTarget(label, ByteArray(0)), modifiedFilepaths)
      if (digest != null) tracked += label to digest
    }
    if (tracked.isEmpty()) return null
    return sha256 {
      for ((label, digest) in tracked) {
        safePutBytes(label.toByteArray())
        safePutBytes(digest)
      }
    }
  }
}
