package com.bazel_diff.di

import com.bazel_diff.bazel.BazelClient
import com.bazel_diff.bazel.BazelModService
import com.bazel_diff.bazel.BazelQueryService
import com.bazel_diff.bazel.ModuleGraphParser
import com.bazel_diff.hash.*
import com.bazel_diff.io.ContentHashProvider
import com.bazel_diff.log.Logger
import com.bazel_diff.log.StderrLogger
import com.bazel_diff.process.Redirect
import com.bazel_diff.process.process
import com.google.gson.GsonBuilder
import java.io.File
import java.nio.file.Path
import java.nio.file.Paths
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import org.koin.core.module.Module
import org.koin.core.qualifier.named
import org.koin.dsl.module

@OptIn(ExperimentalCoroutinesApi::class)
fun hasherModule(
    workingDirectory: Path,
    bazelPath: Path,
    contentHashPath: File?,
    startupOptions: List<String>,
    commandOptions: List<String>,
    cqueryOptions: List<String>,
    useCquery: Boolean,
    cqueryExpression: String?,
    keepGoing: Boolean,
    trackDeps: Boolean,
    fineGrainedHashExternalRepos: Set<String>,
    fineGrainedHashExternalReposFile: File?,
    excludeExternalTargets: Boolean,
): Module = module {
  if (fineGrainedHashExternalReposFile != null && fineGrainedHashExternalRepos.isNotEmpty()) {
    System.err.println(
        "Error: fineGrainedHashExternalReposFile and fineGrainedHashExternalRepos are mutually exclusive - please provide only one of them")
    System.exit(1)
  }
  val userSuppliedFineGrainedHashExternalRepos =
      fineGrainedHashExternalReposFile?.let { file ->
        file.readLines().filter { it.isNotBlank() }.toSet()
      } ?: fineGrainedHashExternalRepos

  // Auto-expand the user-supplied fine-grained set with any bzlmod module that transitively
  // wraps one of those repos (issue #197). Without this, a main-repo target consuming
  // `@middle_repo//:wrapped` -- itself an alias for `@inner_repo//:all_files` -- collapses
  // `@middle_repo` into an opaque `//external:middle_repo` blob, so source changes inside
  // `@inner_repo` never reach the main consumer. Adding the wrapper to the fine-grained set
  // keeps `@middle_repo//:wrapped` as a real ruleInput whose `actual` attribute carries the
  // chain down to the leaf.
  val updatedFineGrainedHashExternalRepos =
      expandFineGrainedHashExternalReposWithBzlmodDependents(
          userSuppliedFineGrainedHashExternalRepos,
          workingDirectory,
          bazelPath,
          startupOptions,
      )

  val cmd: MutableList<String> =
      ArrayList<String>().apply {
        add(bazelPath.toString())
        addAll(startupOptions)
        add("info")
        add("output_base")
      }
  val result = runBlocking {
    process(
        *cmd.toTypedArray(),
        stdout = Redirect.CAPTURE,
        workingDirectory = workingDirectory.toFile(),
        stderr = Redirect.PRINT,
        destroyForcibly = true,
    )
  }
  val outputPath = Paths.get(result.output.single())
  val debug = System.getProperty("DEBUG", "false").equals("true")
  single {
    BazelQueryService(
        workingDirectory,
        bazelPath,
        startupOptions,
        commandOptions,
        cqueryOptions,
        keepGoing,
        debug)
  }
  single {
    BazelModService(workingDirectory, bazelPath, startupOptions, debug)
  }
  single { BazelClient(useCquery, cqueryExpression, updatedFineGrainedHashExternalRepos, excludeExternalTargets) }
  single { BuildGraphHasher(get()) }
  single { TargetHasher() }
  single { RuleHasher(useCquery, trackDeps, updatedFineGrainedHashExternalRepos) }
  single<SourceFileHasher> { SourceFileHasherImpl(updatedFineGrainedHashExternalRepos) }
  single { ExternalRepoResolver(workingDirectory, bazelPath, outputPath) }
  single(named("working-directory")) { workingDirectory }
  single(named("output-base")) { outputPath }
  single { ContentHashProvider(contentHashPath) }
}

/**
 * Returns [userSupplied] together with every bzlmod module that transitively depends on a repo in
 * [userSupplied]. Each entry is the apparent-name form prefixed with `@`, matching the format
 * consumed by [BazelClient]/[com.bazel_diff.bazel.BazelRule] and the user-facing CLI flag.
 *
 * If `bazel mod graph --output=json` fails (e.g. bzlmod disabled) or the user supplied nothing,
 * this is a no-op and returns [userSupplied] unchanged. This intentionally degrades gracefully
 * for WORKSPACE-only setups where the bzlmod graph is unavailable -- the wrapped-repo bug
 * (issue #197) is bzlmod-specific anyway.
 */
@OptIn(ExperimentalCoroutinesApi::class)
private fun expandFineGrainedHashExternalReposWithBzlmodDependents(
    userSupplied: Set<String>,
    workingDirectory: Path,
    bazelPath: Path,
    startupOptions: List<String>,
): Set<String> {
  if (userSupplied.isEmpty()) return userSupplied

  val cmd =
      mutableListOf<String>().apply {
        add(bazelPath.toString())
        addAll(startupOptions)
        add("mod")
        add("graph")
        add("--output=json")
      }
  val result =
      runBlocking {
        process(
            *cmd.toTypedArray(),
            stdout = Redirect.CAPTURE,
            stderr = Redirect.SILENT,
            workingDirectory = workingDirectory.toFile(),
            destroyForcibly = true,
        )
      }
  if (result.resultCode != 0) return userSupplied
  val json = result.output.joinToString("\n").trim()
  if (json.isEmpty()) return userSupplied

  val parser = ModuleGraphParser()
  val graph = parser.parseModuleGraphEdges(json)
  if (graph.edges.isEmpty()) return userSupplied

  // The user-supplied set uses the `@apparentName` form; strip leading `@`/`@@` to match the
  // apparent names emitted by `bazel mod graph`.
  val apparentTargets =
      userSupplied
          .map { it.removePrefix("@@").removePrefix("@") }
          .filter { it.isNotEmpty() }
          .toSet()
  val dependents =
      parser.findTransitiveDependents(graph.edges, apparentTargets, graph.rootApparentNames)
  if (dependents.isEmpty()) return userSupplied

  return userSupplied + dependents.map { "@$it" }
}

fun loggingModule(verbose: Boolean) = module { single<Logger> { StderrLogger(verbose) } }

fun serialisationModule() = module {
  single { GsonBuilder().disableHtmlEscaping().setPrettyPrinting().create() }
}
