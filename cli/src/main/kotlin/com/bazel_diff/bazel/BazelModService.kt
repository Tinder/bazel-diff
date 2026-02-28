package com.bazel_diff.bazel

import com.bazel_diff.log.Logger
import com.bazel_diff.process.Redirect
import com.bazel_diff.process.process
import java.nio.file.Path
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

/**
 * Service that runs `bazel mod` to detect whether Bzlmod is enabled in the workspace.
 * Used to decide whether to query //external:all-targets (disabled when Bzlmod is active).
 */
class BazelModService(
    private val workingDirectory: Path,
    private val bazelPath: Path,
    private val startupOptions: List<String>,
    private val noBazelrc: Boolean,
) : KoinComponent {
  private val logger: Logger by inject()

  /** True if Bzlmod is enabled (e.g. `bazel mod graph` succeeds). When true, //external is not available. */
  val isBzlmodEnabled: Boolean by lazy { runBlocking { checkBzlmodEnabled() } }

  /**
   * Returns the module dependency graph as a string for hashing purposes.
   * This captures all module dependencies and their versions, allowing bazel-diff to detect
   * when MODULE.bazel changes (e.g., when a module version is updated).
   *
   * @return The output of `bazel mod graph` if bzlmod is enabled, or null if disabled/error.
   */
  suspend fun getModuleGraph(): String? {
    if (!isBzlmodEnabled) {
      return null
    }

    val cmd =
        mutableListOf<String>().apply {
          add(bazelPath.toString())
          if (noBazelrc) {
            add("--bazelrc=/dev/null")
          }
          addAll(startupOptions)
          add("mod")
          add("graph")
        }
    logger.i { "Executing Bazel mod graph for hashing: ${cmd.joinToString()}" }
    val result =
        process(
            *cmd.toTypedArray(),
            stdout = Redirect.CAPTURE,
            stderr = Redirect.CAPTURE,
            workingDirectory = workingDirectory.toFile(),
            destroyForcibly = true,
        )

    return if (result.resultCode == 0) {
      result.output.joinToString("\n").trim()
    } else {
      logger.w { "Failed to get module graph" }
      null
    }
  }

  @OptIn(ExperimentalCoroutinesApi::class)
  private suspend fun checkBzlmodEnabled(): Boolean {
    val cmd =
        mutableListOf<String>().apply {
          add(bazelPath.toString())
          if (noBazelrc) {
            add("--bazelrc=/dev/null")
          }
          addAll(startupOptions)
          add("mod")
          add("graph")
        }
    logger.i { "Executing Bazel mod graph: ${cmd.joinToString()}" }
    val result =
        process(
            *cmd.toTypedArray(),
            stdout = Redirect.CAPTURE,
            stderr = Redirect.CAPTURE,
            workingDirectory = workingDirectory.toFile(),
            destroyForcibly = true,
        )
    return result.resultCode == 0
  }
}
