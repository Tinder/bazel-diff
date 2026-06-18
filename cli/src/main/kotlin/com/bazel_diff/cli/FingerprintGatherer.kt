package com.bazel_diff.cli

import com.bazel_diff.interactor.FingerprintInputs
import java.nio.file.Files
import java.nio.file.Path
import java.util.concurrent.TimeUnit

/**
 * Gathers the environment-specific [FingerprintInputs] (reads `MODULE.bazel.lock`, `.bazelrc` +
 * imports, runs `bazel version`, reads bazel-diff's version) and canonicalizes the query-affecting
 * flag set.
 *
 * Shared by [FingerprintCommand] and [WarmupCommand] so the cache key is computed identically on
 * the record and consume sides. The pure hashing lives in
 * [com.bazel_diff.interactor.FingerprintInteractor]; this object is the IO around it.
 */
object FingerprintGatherer {

  /** Canonicalize the flag set into stable key -> value strings. Lists are joined, sets sorted. */
  fun canonicalizeFlags(
      bazelStartupOptions: List<String>,
      bazelCommandOptions: List<String>,
      cqueryCommandOptions: List<String>,
      useCquery: Boolean,
      cqueryExpression: String?,
      includeTargetType: Boolean,
      targetType: Set<String>?,
      fineGrainedHashExternalRepos: Set<String>,
      ignoredRuleHashingAttributes: Set<String>,
      excludeExternalTargets: Boolean,
      keepGoing: Boolean,
  ): Map<String, String> {
    fun list(xs: List<String>) = xs.joinToString(" ")
    fun set(xs: Set<String>) = xs.toSortedSet().joinToString(",")
    return linkedMapOf(
        "bazelStartupOptions" to list(bazelStartupOptions),
        "bazelCommandOptions" to list(bazelCommandOptions),
        "cqueryCommandOptions" to list(cqueryCommandOptions),
        "useCquery" to useCquery.toString(),
        "cqueryExpression" to (cqueryExpression ?: ""),
        "includeTargetType" to includeTargetType.toString(),
        "targetType" to (targetType?.let { set(it) } ?: ""),
        "fineGrainedHashExternalRepos" to set(fineGrainedHashExternalRepos),
        "ignoredRuleHashingAttributes" to set(ignoredRuleHashingAttributes),
        "excludeExternalTargets" to excludeExternalTargets.toString(),
        "keepGoing" to keepGoing.toString(),
    )
  }

  fun gather(
      workspacePath: Path,
      bazelPath: Path,
      bazelDiffVersion: String,
      flags: Map<String, String>,
  ): FingerprintInputs =
      FingerprintInputs(
          bazelDiffVersion = bazelDiffVersion,
          bazelVersion = readBazelVersion(workspacePath, bazelPath),
          moduleLockContent = readOptional(workspacePath.resolve("MODULE.bazel.lock")),
          bazelrcContents = readBazelrcs(workspacePath),
          flags = flags,
      )

  private fun readOptional(path: Path): ByteArray? =
      if (Files.isRegularFile(path)) Files.readAllBytes(path) else null

  /** Read `.bazelrc` plus any files it `import`s / `try-import`s (best-effort, one level deep). */
  private fun readBazelrcs(workspacePath: Path): Map<String, ByteArray> {
    val result = LinkedHashMap<String, ByteArray>()
    val root = workspacePath.resolve(".bazelrc")
    readOptional(root)?.let { bytes ->
      result[".bazelrc"] = bytes
      String(bytes).lineSequence().forEach { line ->
        val trimmed = line.trim()
        if (trimmed.startsWith("import ") || trimmed.startsWith("try-import ")) {
          val raw = trimmed.substringAfter(' ').trim()
          val resolved = raw.replace("%workspace%", workspacePath.toString())
          val p = Path.of(resolved)
          val abs = if (p.isAbsolute) p else workspacePath.resolve(resolved)
          readOptional(abs)?.let { imported ->
            result[workspacePath.relativize(abs).toString()] = imported
          }
        }
      }
    }
    return result
  }

  private fun readBazelVersion(workspacePath: Path, bazelPath: Path): String {
    return try {
      val proc =
          ProcessBuilder(bazelPath.toString(), "version")
              .directory(workspacePath.toFile())
              .redirectErrorStream(false)
              .start()
      val output = proc.inputStream.bufferedReader().readText()
      proc.waitFor(60, TimeUnit.SECONDS)
      output
          .lineSequence()
          .firstOrNull { it.startsWith("Build label: ") }
          ?.removePrefix("Build label: ")
          ?.trim()
          ?: output
              .lineSequence()
              .firstOrNull { it.startsWith("bazel ") }
              ?.removePrefix("bazel ")
              ?.trim()
          ?: "unknown"
    } catch (e: Exception) {
      "unknown"
    }
  }
}
