package com.bazel_diff.bazel

import com.bazel_diff.log.Logger
import com.bazel_diff.process.Redirect
import com.bazel_diff.process.process
import com.google.devtools.build.lib.analysis.AnalysisProtosV2
import com.google.devtools.build.lib.query2.proto.proto2api.Build
import java.io.File
import java.nio.file.Files
import java.nio.file.Path
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

class BazelQueryService(
    private val workingDirectory: Path,
    private val bazelPath: Path,
    private val startupOptions: List<String>,
    private val commandOptions: List<String>,
    private val cqueryOptions: List<String>,
    private val keepGoing: Boolean,
    private val noBazelrc: Boolean,
) : KoinComponent {
  private val logger: Logger by inject()

  suspend fun query(query: String, useCquery: Boolean = false): List<BazelTarget> {
    // Unfortunately, there is still no direct way to tell if a target is compatible or not with the
    // proto output
    // by itself. So we do an extra cquery with the trick at
    // https://bazel.build/extending/platforms#cquery-incompatible-target-detection to first find
    // all compatible
    // targets.
    val compatibleTargetSet =
        if (useCquery) {
          runQuery(query, useCquery = true, outputCompatibleTargets = true).useLines {
            it.filter { it.isNotBlank() }.toSet()
          }
        } else {
          emptySet()
        }
    val outputFile = runQuery(query, useCquery)

    val targets =
        outputFile.inputStream().buffered().use { proto ->
          if (useCquery) {
            val cqueryResult = AnalysisProtosV2.CqueryResult.parseFrom(proto)
            cqueryResult.resultsList
                .mapNotNull { toBazelTarget(it.target) }
                .filter { it.name in compatibleTargetSet }
          } else {
            mutableListOf<Build.Target>()
                .apply {
                  while (true) {
                    val target = Build.Target.parseDelimitedFrom(proto) ?: break
                    // EOF
                    add(target)
                  }
                }
                .mapNotNull { toBazelTarget(it) }
          }
        }

    return targets
  }

  @OptIn(ExperimentalCoroutinesApi::class)
  private suspend fun runQuery(
      query: String,
      useCquery: Boolean,
      outputCompatibleTargets: Boolean = false
  ): File {
    val queryFile = Files.createTempFile(null, ".txt").toFile()
    queryFile.deleteOnExit()
    val outputFile = Files.createTempFile(null, ".bin").toFile()
    outputFile.deleteOnExit()

    queryFile.writeText(query)

    val cmd: MutableList<String> =
        ArrayList<String>().apply {
          add(bazelPath.toString())
          if (noBazelrc) {
            add("--bazelrc=/dev/null")
          }
          addAll(startupOptions)
          if (useCquery) {
            add("cquery")
            if (!outputCompatibleTargets) {
              // There is no need to query the transitions when querying for compatible targets.
              add("--transitions=lite")
            }
          } else {
            add("query")
          }
          add("--output")
          if (useCquery) {
            if (outputCompatibleTargets) {
              add("starlark")
              add("--starlark:file")
              val cqueryOutputFile = Files.createTempFile(null, ".cquery").toFile()
              cqueryOutputFile.deleteOnExit()
              cqueryOutputFile.writeText(
                  """
                    def format(target):
                        if providers(target) == None:
                            return ""
                        if "IncompatiblePlatformProvider" not in providers(target):
                            target_repr = repr(target)
                            if "<alias target" in target_repr:
                                return target_repr.split(" ")[2]
                            return str(target.label)
                        return ""
                    """
                      .trimIndent())
              add(cqueryOutputFile.toString())
            } else {
              // Unfortunately, cquery does not support streamed_proto yet.
              // See https://github.com/bazelbuild/bazel/issues/17743. This poses an issue for large
              // monorepos.
              add("proto")
            }
          } else {
            add("streamed_proto")
          }
          if (!useCquery) {
            add("--order_output=no")
          }
          if (keepGoing) {
            add("--keep_going")
          }
          if (useCquery) {
            addAll(cqueryOptions)
            add("--consistent_labels")
          } else {
            addAll(commandOptions)
          }
          add("--query_file")
          add(queryFile.toString())
        }

    logger.i { "Executing Query: $query" }
    logger.i { "Command: ${cmd.toTypedArray().joinToString()}" }
    val result = runBlocking {
      process(
          *cmd.toTypedArray(),
          stdout = Redirect.ToFile(outputFile),
          workingDirectory = workingDirectory.toFile(),
          stderr = Redirect.PRINT,
          destroyForcibly = true,
      )
    }

    if (result.resultCode != 0)
        throw RuntimeException("Bazel query failed, exit code ${result.resultCode}")
    return outputFile
  }

  private fun toBazelTarget(target: Build.Target): BazelTarget? {
    return when (target.type) {
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
