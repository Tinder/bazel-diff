package com.bazel_diff.bazel

import com.bazel_diff.log.Logger
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import java.nio.file.Path
import java.util.*
import org.koin.core.component.inject
import org.koin.core.component.KoinComponent

class BazelClient(private val useCquery: Boolean, private val fineGrainedHashExternalRepos: Set<String>) : KoinComponent {
    private val logger: Logger by inject()
    private val queryService: BazelQueryService by inject()

    suspend fun queryAllTargets(): List<BazelTarget> {
        val queryEpoch = Calendar.getInstance().getTimeInMillis()

        val repoTargetsQuery = listOf("//external:all-targets")
        val targets = if (useCquery) {
            // Explicitly listing external repos here sometimes causes issues mentioned at
            // https://bazel.build/query/cquery#recursive-target-patterns. Hence, we query all dependencies with `deps`
            // instead. However, we still need to append all "//external:*" targets because fine-grained hash
            // computation depends on hashing of source files in external repos as well, which is limited to repos
            // explicitly mentioned in `fineGrainedHashExternalRepos` flag. Therefore, for any repos not mentioned there
            // we are still relying on the repo-generation target under `//external` to compute the hash.
            //
            // In addition, we must include all source dependencies in this query in order for them to show up in
            // `configuredRuleInput`. Hence, one must not filter them out with `kind(rule, deps(..))`. However, these
            // source targets are omitted inside BazelQueryService with the custom starlark function used to print
            // labels.
            (queryService.query("deps(//...:all-targets)", useCquery = true) +
                queryService.query(repoTargetsQuery.joinToString(" + ") { "'$it'" }))
                .distinctBy { it.rule.name }
        } else {
            val buildTargetsQuery = listOf("//...:all-targets") + fineGrainedHashExternalRepos.map { "@$it//...:all-targets" }
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
}
