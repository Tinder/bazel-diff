package com.bazel_diff.bazel

import com.bazel_diff.extensions.toHexString
import com.bazel_diff.hash.sha256
import com.bazel_diff.log.Logger
import com.bazel_diff.process.Redirect
import com.bazel_diff.process.process
import java.io.File
import java.nio.charset.StandardCharsets
import java.nio.file.Path
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject

/**
 * Service that runs `bazel mod` to detect whether Bzlmod is enabled in the workspace. Used to
 * decide whether to query //external:all-targets (disabled when Bzlmod is active).
 */
class BazelModService(
    private val workingDirectory: Path,
    private val bazelPath: Path,
    private val startupOptions: List<String>,
    private val noBazelrc: Boolean,
) : KoinComponent {
  private val logger: Logger by inject()

  /**
   * True if Bzlmod is enabled (e.g. `bazel mod graph` succeeds). When true, //external is not
   * available.
   */
  val isBzlmodEnabled: Boolean by lazy { runBlocking { checkBzlmodEnabled() } }

  /**
   * Returns the module dependency graph as a string for hashing purposes. This captures all module
   * dependencies and their versions, allowing bazel-diff to detect when MODULE.bazel changes (e.g.,
   * when a module version is updated).
   *
   * @return The output of `bazel mod graph` if bzlmod is enabled, or null if disabled/error.
   */
  @OptIn(ExperimentalCoroutinesApi::class)
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
            stderr = Redirect.SILENT,
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

  /**
   * Returns the module dependency graph in JSON format for precise change detection.
   *
   * @return The JSON output of `bazel mod graph --output=json` if bzlmod is enabled, or null if
   *   disabled/error.
   */
  @OptIn(ExperimentalCoroutinesApi::class)
  suspend fun getModuleGraphJson(): String? {
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
          add("--output=json")
        }
    logger.i { "Executing Bazel mod graph JSON: ${cmd.joinToString()}" }
    val result =
        process(
            *cmd.toTypedArray(),
            stdout = Redirect.CAPTURE,
            stderr = Redirect.SILENT,
            workingDirectory = workingDirectory.toFile(),
            destroyForcibly = true,
        )

    return if (result.resultCode == 0) {
      result.output.joinToString("\n").trim()
    } else {
      logger.w { "Failed to get module graph JSON" }
      null
    }
  }

  /**
   * Computes a stable fingerprint of the currently resolved external dependency state.
   *
   * The hash includes:
   * - bzlmod mode marker + `bazel mod graph --output=json`
   * - repository-definition bytes from `bazel mod show_repo` (streamed proto when available)
   */
  suspend fun getDependencyFingerprint(): String? {
    if (!isBzlmodEnabled) {
      return sha256 { putBytes("mode:legacy".toByteArray(StandardCharsets.UTF_8)) }.toHexString()
    }

    val moduleGraphJson = getModuleGraphJson() ?: ""
    val canonicalRepos = discoverCanonicalBzlmodRepos()

    val streamedShowRepo = showRepoStreamedProto(canonicalRepos)
    if (streamedShowRepo != null && streamedShowRepo.exitCode == 0) {
      return sha256 {
            putBytes("mode:bzlmod\n".toByteArray(StandardCharsets.UTF_8))
            putBytes("moduleGraphJson:".toByteArray(StandardCharsets.UTF_8))
            putBytes(moduleGraphJson.toByteArray(StandardCharsets.UTF_8))
            putBytes("\nshowRepo:\n".toByteArray(StandardCharsets.UTF_8))
            putBytes(streamedShowRepo.stdout)
          }
          .toHexString()
    }

    val showRepoText = resolveShowRepoTextFallback(canonicalRepos) ?: return null
    return sha256 {
          putBytes("mode:bzlmod\n".toByteArray(StandardCharsets.UTF_8))
          putBytes("moduleGraphJson:".toByteArray(StandardCharsets.UTF_8))
          putBytes(moduleGraphJson.toByteArray(StandardCharsets.UTF_8))
          putBytes("\nshowRepoText:\n".toByteArray(StandardCharsets.UTF_8))
          putBytes(showRepoText.toByteArray(StandardCharsets.UTF_8))
        }
        .toHexString()
  }

  /**
   * Returns canonical bzlmod repo names in @@<canonical> form, discovered from `bazel mod
   * dump_repo_mapping ""`.
   */
  private fun discoverCanonicalBzlmodRepos(): List<String> {
    val output = runBazelRaw(listOf("mod", "dump_repo_mapping", "")) ?: return emptyList()
    if (output.exitCode != 0) {
      return emptyList()
    }
    return String(output.stdout, StandardCharsets.UTF_8)
        .lineSequence()
        .mapNotNull { line -> parseCanonicalRepoNames(line) }
        .flatten()
        .filter { it.contains('+') || it.contains('~') }
        .map { "@@$it" }
        .toSet()
        .sorted()
  }

  private fun parseCanonicalRepoNames(line: String): List<String>? {
    val parsed = runCatching {
      @Suppress("UNCHECKED_CAST")
      com.google.gson.Gson().fromJson(line.trim(), Map::class.java) as Map<String, Any?>
    }
    if (parsed.isFailure) return null
    return parsed.getOrNull()?.values?.mapNotNull { it as? String }
  }

  private fun showRepoStreamedProto(canonicalRepos: List<String>): RawCommandResult? {
    val args = mutableListOf("mod", "show_repo")
    if (canonicalRepos.isNotEmpty()) {
      args.addAll(canonicalRepos)
    }
    args.add("--output=streamed_proto")
    return runBazelRaw(args)
  }

  private fun resolveShowRepoTextFallback(canonicalRepos: List<String>): String? {
    val allVisible = runBazelRaw(listOf("mod", "show_repo", "--all_visible_repos", "--output=text"))
    if (allVisible != null && allVisible.exitCode == 0) {
      return String(allVisible.stdout, StandardCharsets.UTF_8)
    }

    val args = mutableListOf("mod", "show_repo")
    if (canonicalRepos.isNotEmpty()) {
      args.addAll(canonicalRepos)
    }
    args.add("--output=text")
    val fallback = runBazelRaw(args) ?: return null
    if (fallback.exitCode != 0) {
      return null
    }
    return String(fallback.stdout, StandardCharsets.UTF_8)
  }

  private data class RawCommandResult(
      val exitCode: Int,
      val stdout: ByteArray,
  )

  private fun runBazelRaw(args: List<String>): RawCommandResult? {
    val command =
        mutableListOf<String>().apply {
          add(bazelPath.toString())
          if (noBazelrc) {
            add("--bazelrc=/dev/null")
          }
          addAll(startupOptions)
          addAll(args)
        }
    return try {
      val nullDevice =
          if (System.getProperty("os.name").startsWith("Windows")) "NUL" else "/dev/null"
      val process =
          ProcessBuilder(command)
              .directory(workingDirectory.toFile())
              .redirectError(ProcessBuilder.Redirect.to(File(nullDevice)))
              .start()
      val stdout = process.inputStream.readBytes()
      val exitCode = process.waitFor()
      if (exitCode != 0) {
        logger.w { "Command failed (exit=$exitCode): ${command.joinToString(" ")}" }
      }
      RawCommandResult(exitCode = exitCode, stdout = stdout)
    } catch (e: Exception) {
      logger.w { "Failed to execute ${command.joinToString(" ")}: ${e.message}" }
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
