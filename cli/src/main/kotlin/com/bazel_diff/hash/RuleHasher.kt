package com.bazel_diff.hash

import com.bazel_diff.bazel.BazelRule
import com.bazel_diff.bazel.BazelSourceFileTarget
import com.bazel_diff.bazel.decodeConfiguredRuleInputLabel
import com.bazel_diff.log.Logger
import com.google.common.annotations.VisibleForTesting
import java.nio.file.Path
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.ConcurrentMap
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

class RuleHasher(
    private val useCquery: Boolean,
    private val trackDepLabels: Boolean,
    private val fineGrainedHashExternalRepos: Set<String>,
    private val alwaysAffectedTags: Set<String> = emptySet(),
    private val alwaysAffectedSeed: ByteArray = ByteArray(0),
) : KoinComponent {
  private val logger: Logger by inject()
  private val sourceFileHasher: SourceFileHasher by inject()

  // main-repo `.bzl` path -> content digest; EMPTY marks a non-main-repo/missing file (skip).
  private val bzlDigestCache = ConcurrentHashMap<String, ByteArray>()
  private val EMPTY = ByteArray(0)

  /**
   * Per-rule `.bzl` seed: digests of the main-repo `.bzl` files in this rule's macro instantiation
   * stack, so a macro edit re-hashes only the rules that macro produced (issue #365).
   * `$rule_implementation_hash` (in [BazelRule.digest]) already covers a rule class's own
   * definition `.bzl`. Returns null only when the rule has no stack (`--proto:instantiation_stack`
   * off) so the caller falls back to the package seed; a macro-less rule returns a stable constant,
   * so the caller does NOT fall back.
   */
  private fun ruleBzlSeed(rule: BazelRule, modifiedFilepaths: Set<Path>): ByteArray? {
    val stack = rule.instantiationStack
    if (stack.isEmpty()) return null
    val bzlPaths = sortedSetOf<String>()
    for (frame in stack) {
      val path = frame.substringBefore(":") // "tools/x.bzl:12:3: macro" -> "tools/x.bzl"
      if ((path.endsWith(".bzl") || path.endsWith(".scl")) &&
          !path.startsWith("external/") &&
          !path.startsWith("@") &&
          !path.startsWith("../")) {
        bzlPaths.add(path)
      }
    }
    return sha256 {
      for (path in bzlPaths) {
        val digest =
            bzlDigestCache.computeIfAbsent(path) {
              sourceFileHasher.softDigest(
                  BazelSourceFileTarget("//$it", ByteArray(0)), modifiedFilepaths) ?: EMPTY
            }
        if (digest.isNotEmpty()) {
          safePutBytes(path.toByteArray())
          safePutBytes(digest)
        }
      }
    }
  }

  @VisibleForTesting class CircularDependencyException(message: String) : Exception(message)

  private fun raiseCircularDependency(
      depPath: LinkedHashSet<String>,
      begin: String
  ): CircularDependencyException {
    val sb =
        StringBuilder().appendLine("Circular dependency detected: ").append(begin).append(" -> ")
    val circularPath = depPath.toList().takeLastWhile { it != begin }
    circularPath.forEach { sb.append(it).append(" -> ") }
    sb.append(begin)
    return CircularDependencyException(sb.toString())
  }

  fun digest(
      rule: BazelRule,
      allRulesMap: Map<String, BazelRule>,
      ruleHashes: ConcurrentMap<String, TargetDigest>,
      sourceDigests: ConcurrentMap<String, ByteArray>,
      seedHash: ByteArray?,
      packageBzlSeeds: Map<String, ByteArray>,
      depPath: LinkedHashSet<String>?,
      ignoredAttrs: Set<String>,
      modifiedFilepaths: Set<Path>
  ): TargetDigest {
    val depPathClone = if (depPath != null) LinkedHashSet(depPath) else LinkedHashSet()
    if (depPathClone.contains(rule.name)) {
      throw raiseCircularDependency(depPathClone, rule.name)
    }
    depPathClone.add(rule.name)
    ruleHashes[rule.name]?.let {
      return it
    }

    // issue #401: a target carrying an --alwaysAffectedTags tag (e.g. `external`) may read
    // undeclared workspace state at execution time, so its *declared* inputs can't make its hash
    // change when that state does, and target determination wrongly skips it. Mix a per-invocation
    // sentinel into its digest so two generate-hashes runs never agree and a base/head diff always
    // marks it impacted. Only tagged rules are touched -- every other target's hash stays
    // byte-for-byte identical to a run without the flag.
    val alwaysAffected =
        alwaysAffectedTags.isNotEmpty() && rule.tags().any { it in alwaysAffectedTags }

    val finalHashValue =
        targetSha256(trackDepLabels) {
          putDirectBytes(rule.digest(ignoredAttrs))
          putDirectBytes(seedHash)
          // Per-rule macro seed (see ruleBzlSeed), else the package-wide fallback. Each rule
          // resolves its own seed, so this is stable under the memoized recursion below (#365).
          putDirectBytes(
              ruleBzlSeed(rule, modifiedFilepaths) ?: packageBzlSeeds[labelToPackage(rule.name)])
          // Mixed into the *direct* digest (not transitively) so the tagged target is classified
          // as DIRECT-impacted for distance metrics; it still bubbles into the overall digest, so
          // any rdeps are conservatively re-hashed too.
          if (alwaysAffected) {
            putDirectBytes(alwaysAffectedSeed)
          }

          for (ruleInput in rule.ruleInputList(useCquery, fineGrainedHashExternalRepos)) {
            // Under --useCquery, `ruleInput` may carry an embedded configurationChecksum (see
            // BazelRule.CONFIGURED_RULE_INPUT_SEPARATOR / #359). The full encoded string is what
            // we mix into the hash so two configured graphs differing only by per-edge
            // configuration produce distinct digests; the bare label is what we use to look up
            // the input in `allRulesMap` / `sourceDigests` and to track in `deps`.
            putDirectBytes(ruleInput.toByteArray())
            val inputLabel = decodeConfiguredRuleInputLabel(ruleInput)

            val inputRule = allRulesMap[inputLabel]
            when {
              inputRule == null && sourceDigests.containsKey(inputLabel) -> {
                putDirectBytes(sourceDigests[inputLabel])
              }
              inputRule?.name != null && inputRule.name != rule.name -> {
                val ruleInputHash =
                    digest(
                        inputRule,
                        allRulesMap,
                        ruleHashes,
                        sourceDigests,
                        seedHash,
                        packageBzlSeeds,
                        depPathClone,
                        ignoredAttrs,
                        modifiedFilepaths)
                putTransitiveBytes(inputLabel, ruleInputHash.overallDigest)
              }
              else -> {
                val heuristicDigest =
                    sourceFileHasher.softDigest(
                        BazelSourceFileTarget(inputLabel, ByteArray(0)), modifiedFilepaths)
                when {
                  heuristicDigest != null -> {
                    logger.i {
                      "Source file $inputLabel picked up as an input for rule ${rule.name}"
                    }
                    sourceDigests[inputLabel] = heuristicDigest
                    putDirectBytes(heuristicDigest)
                  }
                  else ->
                      logger.w {
                        "Unable to calculate digest for input $inputLabel for rule ${rule.name}"
                      }
                }
              }
            }
          }
        }

    return finalHashValue.also { ruleHashes[rule.name] = it }
  }
}
