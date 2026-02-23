package com.bazel_diff.di

import com.bazel_diff.bazel.BazelClient
import com.bazel_diff.bazel.BazelModService
import com.bazel_diff.bazel.BazelQueryService
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
  val updatedFineGrainedHashExternalRepos =
      fineGrainedHashExternalReposFile?.let { file ->
        file.readLines().filter { it.isNotBlank() }.toSet()
      } ?: fineGrainedHashExternalRepos

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

fun loggingModule(verbose: Boolean) = module { single<Logger> { StderrLogger(verbose) } }

fun serialisationModule() = module {
  single { GsonBuilder().disableHtmlEscaping().setPrettyPrinting().create() }
}
