package com.bazel_diff.bazel

import com.bazel_diff.log.Logger
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import java.util.*

class BazelClient(private val useCquery: Boolean, private val fineGrainedHashExternalRepos: Set<String>) : KoinComponent {
    private val logger: Logger by inject()
    private val queryService: BazelQueryService by inject()

    suspend fun queryAllTargets(): List<BazelTarget> {
        val queryEpoch = Calendar.getInstance().getTimeInMillis()

        val repoTargetsQuery = listOf("//external:all-targets")
        val buildTargetsQuery = listOf("//...:all-targets") + fineGrainedHashExternalRepos.map { "@$it//...:all-targets" }
        val targets = if (useCquery) {
            (queryService.query(buildTargetsQuery.joinToString(" + ") { "'$it'" }, useCquery = true) +
                queryService.query(repoTargetsQuery.joinToString(" + ") { "'$it'" }))
                .distinctBy { it.rule.name }
        } else {
            queryService.query((repoTargetsQuery + buildTargetsQuery).joinToString(" + ") { "'$it'" })
        }
        val queryDuration = Calendar.getInstance().getTimeInMillis() - queryEpoch
        logger.i { "All targets queried in $queryDuration" }
        return targets.mapNotNull { target: Build.Target ->
            when (target.type) {
                Build.Target.Discriminator.RULE -> BazelTarget.Rule(target)
                Build.Target.Discriminator.SOURCE_FILE -> BazelTarget.SourceFile(
                    target
                )

                Build.Target.Discriminator.GENERATED_FILE -> BazelTarget.GeneratedFile(
                    target
                )

                else -> {
                    logger.w { "Unsupported target type in the build graph: ${target.type.name}" }
                    null
                }
            }
        }
    }

    suspend fun queryAllSourcefileTargets(): List<Build.Target> {
        val queryEpoch = Calendar.getInstance().getTimeInMillis()
        val allReposToQuery = listOf("@") + fineGrainedHashExternalRepos.map { "@$it" }
        val targets = queryService.query("kind('source file', ${allReposToQuery.joinToString(" + ") { "'$it//...:all-targets'" }})")
        val queryDuration = Calendar.getInstance().getTimeInMillis() - queryEpoch
        logger.i { "All source files queried in $queryDuration" }

        return targets
    }
}

