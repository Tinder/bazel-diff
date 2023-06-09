package com.bazel_diff.hash

import com.bazel_diff.bazel.BazelClient
import com.bazel_diff.bazel.BazelRule
import com.bazel_diff.bazel.BazelSourceFileTarget
import com.bazel_diff.bazel.BazelTarget
import com.bazel_diff.extensions.toHexString
import com.bazel_diff.log.Logger
import com.google.common.collect.Sets
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import java.nio.file.Path
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.ConcurrentMap
import java.util.concurrent.atomic.AtomicReference
import java.util.stream.Collectors
import kotlin.io.path.readBytes
import java.util.Calendar

class BuildGraphHasher(private val bazelClient: BazelClient) : KoinComponent {
    private val targetHasher: TargetHasher by inject()
    private val sourceFileHasher: SourceFileHasher by inject()
    private val logger: Logger by inject()

    fun hashAllBazelTargetsAndSourcefiles(
        seedFilepaths: Set<Path> = emptySet(),
        ignoredAttrs: Set<String> = emptySet()
    ): Map<String, String> {
        /**
         * Bazel will lock parallel queries but this is still allowing us to hash source files while executing a parallel query
         */
        val (sourceDigests, allTargets) = runBlocking {
            /**
             * Source query is usually faster than targets query, so we prioritise it first
             */
            val sourceTargetsFuture = async(Dispatchers.IO) {
                bazelClient.queryAllSourcefileTargets()
            }
            val sourceTargets = sourceTargetsFuture.await()

            /**
             * Querying targets and source hashing is done in parallel
             */
            val sourceDigestsFuture = async(Dispatchers.IO) {
                val sourceHashDurationEpoch = Calendar.getInstance().getTimeInMillis()
                val sourceFileTargets = hashSourcefiles(sourceTargets)
                val sourceHashDuration = Calendar.getInstance().getTimeInMillis() - sourceHashDurationEpoch
                logger.i { "Source file hashes calculated in $sourceHashDuration" }
                sourceFileTargets
            }
            val targetsTask = async(Dispatchers.IO) {
                bazelClient.queryAllTargets()
            }

            Pair(sourceDigestsFuture.await(), targetsTask.await())
        }
        val seedForFilepaths = createSeedForFilepaths(seedFilepaths)
        return hashAllTargets(
            seedForFilepaths,
            sourceDigests,
            allTargets,
            ignoredAttrs
        )
    }

    private fun hashSourcefiles(targets: List<Build.Target>): ConcurrentMap<String, ByteArray> {
        val exception = AtomicReference<Exception?>(null)
        val result: ConcurrentMap<String, ByteArray> = targets.parallelStream()
            .map { target: Build.Target ->
                target.sourceFile?.let { sourceFile ->
                    val seed = sha256 {
                        safePutBytes(sourceFile.nameBytes.toByteArray())
                        for (subinclude in sourceFile.subincludeList) {
                            safePutBytes(subinclude.toByteArray())
                        }
                    }
                    try {
                        val sourceFileTarget = BazelSourceFileTarget(sourceFile.name, seed)
                        Pair(sourceFileTarget.name, sourceFileHasher.digest(sourceFileTarget))
                    } catch (e: Exception) {
                        exception.set(e)
                        null
                    }
                }
            }
            .filter { pair -> pair != null }
            .collect(
                Collectors.toConcurrentMap(
                    { pair -> pair!!.first },
                    { pair -> pair!!.second },
                )
            )

        exception.get()?.let { throw it }
        return result
    }

    private fun hashAllTargets(
        seedHash: ByteArray,
        sourceDigests: ConcurrentMap<String, ByteArray>,
        allTargets: List<BazelTarget>,
        ignoredAttrs: Set<String>
    ): Map<String, String> {
        val ruleHashes: ConcurrentMap<String, ByteArray> = ConcurrentHashMap()
        val targetToRule: MutableMap<String, BazelRule> = HashMap()
        traverseGraph(allTargets, targetToRule)

        return allTargets.parallelStream()
            .map { target: BazelTarget ->
                val targetDigest = targetHasher.digest(
                    target,
                    targetToRule,
                    sourceDigests,
                    ruleHashes,
                    seedHash,
                    ignoredAttrs
                )
                Pair(target.name, targetDigest.toHexString())
            }
            .filter { targetEntry: Pair<String, String>? -> targetEntry != null }
            .collect(
                Collectors.toMap(
                    { obj: Pair<String, String> -> obj.first },
                    { obj: Pair<String, String> -> obj.second },
                )
            )
    }

    /**
     * Traverses the list of targets and revisits the targets with yet-unknown generating rule
     */
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
                        targetToRule[target.generatingRuleName]?.let {
                            targetToRule[targetName] = it
                        } ?: nextTargets.add(target)
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

    private fun createSeedForFilepaths(seedFilepaths: Set<Path>): ByteArray {
        if (seedFilepaths.isEmpty()) {
            return ByteArray(0)
        }
        return sha256 {
            for (path in seedFilepaths) {
                putBytes(path.readBytes())
            }
        }
    }
}
