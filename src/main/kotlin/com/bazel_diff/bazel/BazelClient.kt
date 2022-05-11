package com.bazel_diff.bazel

import com.bazel_diff.log.Logger
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import java.util.concurrent.ConcurrentMap
import kotlin.time.ExperimentalTime
import kotlin.time.measureTimedValue

class BazelClient : KoinComponent {
    private val logger: Logger by inject()
    private val queryService: BazelQueryService by inject()

    @OptIn(ExperimentalTime::class)
    suspend fun queryAllTargets(): List<BazelTarget> {
        val (targets, queryDuration) = measureTimedValue {
            queryService.query("'//external:all-targets' + '//...:all-targets'")
        }
        logger.i { "All targets queried in $queryDuration" }
        return targets.mapNotNull { target: Build.Target ->
            when (target.type) {
                Build.Target.Discriminator.RULE -> BazelTarget.Rule(target)
                Build.Target.Discriminator.SOURCE_FILE -> BazelTarget.SourceFile(target)
                Build.Target.Discriminator.GENERATED_FILE -> BazelTarget.GeneratedFile(target)
                else -> {
                    logger.w { "Unsupported target type in the build graph: ${target.type.name}" }
                    null
                }
            }
        }
    }

    @OptIn(ExperimentalTime::class)
    suspend fun queryAllSourcefileTargets(): List<Build.Target> {
        val (targets, queryDuration) = measureTimedValue {
            queryService.query("kind('source file', //...:all-targets)")
        }
        logger.i { "All source files queried in $queryDuration" }

        return targets
    }
}

