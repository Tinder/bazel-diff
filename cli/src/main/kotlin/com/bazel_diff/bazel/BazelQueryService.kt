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

private val versionComparator = compareBy<Triple<Int, Int, Int>> { it.first }
    .thenBy { it.second }
    .thenBy { it.third }

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
  private val version: Triple<Int, Int, Int> by lazy {
    runBlocking { determineBazelVersion() }
  }

  @OptIn(ExperimentalCoroutinesApi::class)
  private suspend fun determineBazelVersion(): Triple<Int, Int, Int> {
    val cmd = arrayOf(bazelPath.toString(), "--version")
    logger.i { "Executing Bazel version command: ${cmd.joinToString()}" }
    val result = process(
        *cmd,
        stdout = Redirect.CAPTURE,
        workingDirectory = workingDirectory.toFile(),
        stderr = Redirect.PRINT,
        destroyForcibly = true,
    )

    if (result.resultCode != 0) {
      throw RuntimeException("Bazel version command failed, exit code ${result.resultCode}")
    }

    if (result.output.size != 1 || !result.output.first().startsWith("bazel "))  {
      throw RuntimeException("Bazel version command returned unexpected output: ${result.output}")
    }
    // Trim off any prerelease suffixes.
    val versionString = result.output.first().removePrefix("bazel ").trim().split('-')[0]
    val version = versionString.split('.').map { it.toInt() }.toTypedArray()
    return Triple(version[0], version[1], version[2])
  }

  // Use streamed_proto output for cquery if available. This is more efficient than the proto output.
  // https://github.com/bazelbuild/bazel/commit/607d0f7335f95aa0ee236ba3c18ce2a232370cdb
  private val canUseStreamedProtoWithCquery
    get() = versionComparator.compare(version, Triple(7, 0, 0)) >= 0

  // Use an output file for (c)query if supported. This avoids excessively large stdout, which is sent out on the BES.
  // https://github.com/bazelbuild/bazel/commit/514e9052f2c603c53126fbd9436bdd3ad3a1b0c7
  private val canUseOutputFile
    get() = versionComparator.compare(version, Triple(8, 2, 0)) >= 0

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
            if (canUseStreamedProtoWithCquery) {
              mutableListOf<AnalysisProtosV2.CqueryResult>()
                .apply {
                  while (true) {
                    val result = AnalysisProtosV2.CqueryResult.parseDelimitedFrom(proto) ?: break
                    // EOF
                    add(result)
                  }
                }
                .flatMap { it.resultsList }
            } else {
              AnalysisProtosV2.CqueryResult.parseFrom(proto).resultsList
            }
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

    val allowedExitCodes = mutableListOf(0)

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
              val cqueryStarlarkFile = Files.createTempFile(null, ".cquery").toFile()
              cqueryStarlarkFile.deleteOnExit()
              cqueryStarlarkFile.writeText(
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
                  .trimIndent()
              )
              add(cqueryStarlarkFile.toString())
            } else {
              add(if (canUseStreamedProtoWithCquery) "streamed_proto" else "proto")
            }
          } else {
            add("streamed_proto")
          }
          if (!useCquery) {
            add("--order_output=no")
          }
          if (keepGoing) {
            add("--keep_going")
            allowedExitCodes.add(3)
          }
          if (useCquery) {
            addAll(cqueryOptions)
            add("--consistent_labels")
          } else {
            addAll(commandOptions)
          }
          add("--query_file")
          add(queryFile.toString())
          if (canUseOutputFile) {
            add("--output_file")
            add(outputFile.toString())
          }
        }

    logger.i { "Executing Query: $query" }
    logger.i { "Command: ${cmd.toTypedArray().joinToString()}" }
    val result = process(
          *cmd.toTypedArray(),
          stdout = if (canUseOutputFile) Redirect.SILENT else Redirect.ToFile(outputFile),
          workingDirectory = workingDirectory.toFile(),
          stderr = Redirect.PRINT,
          destroyForcibly = true,
      )

    if (!allowedExitCodes.contains(result.resultCode)) {
        logger.w { "Bazel query failed, output: ${result.output.joinToString("\n")}" }
        throw RuntimeException("Bazel query failed, exit code ${result.resultCode}, allowed exit codes: ${allowedExitCodes.joinToString()}")
    }
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
