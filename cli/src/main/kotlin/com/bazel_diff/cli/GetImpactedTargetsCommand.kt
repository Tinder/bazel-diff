package com.bazel_diff.cli

import com.bazel_diff.cli.converter.NormalisingPathConverter
import com.bazel_diff.cli.converter.OptionsConverter
import com.bazel_diff.di.loggingModule
import com.bazel_diff.di.serialisationModule
import com.bazel_diff.interactor.CalculateImpactedTargetsInteractor
import com.bazel_diff.interactor.DeserialiseHashesInteractor
import java.io.BufferedWriter
import java.io.File
import java.io.FileDescriptor
import java.io.FileWriter
import java.io.IOException
import java.nio.file.Path
import java.util.concurrent.Callable
import org.koin.core.context.startKoin
import org.koin.core.context.stopKoin
import picocli.CommandLine

@CommandLine.Command(
    name = "get-impacted-targets",
    description = ["Command-line utility to analyze the state of the bazel build graph"],
)
class GetImpactedTargetsCommand : Callable<Int> {
  @CommandLine.ParentCommand private lateinit var parent: BazelDiff

  @CommandLine.Option(
      names = ["-sh", "--startingHashes"],
      scope = CommandLine.ScopeType.LOCAL,
      description =
          [
              "The path to the JSON file of target hashes for the initial revision. Run 'generate-hashes' to get this value."],
      required = true,
  )
  lateinit var startingHashesJSONPath: File

  @CommandLine.Option(
      names = ["-fh", "--finalHashes"],
      scope = CommandLine.ScopeType.LOCAL,
      description =
          [
              "The path to the JSON file of target hashes for the final revision. Run 'generate-hashes' to get this value."],
      required = true,
  )
  lateinit var finalHashesJSONPath: File

  @CommandLine.Option(
      names = ["-d", "--depEdgesFile"],
      description =
          [
              "Path to the file where dependency edges are. If specified, build graph distance metrics will be computed from the given hash data."],
      scope = CommandLine.ScopeType.INHERIT,
      defaultValue = CommandLine.Parameters.NULL_VALUE)
  var depsMappingJSONPath: File? = null

  @CommandLine.Option(
      names = ["-tt", "--targetType"],
      split = ",",
      scope = CommandLine.ScopeType.LOCAL,
      description =
          [
              "The types of targets to filter. Use comma (,) to separate multiple values, e.g. '--targetType=SourceFile,Rule,GeneratedFile'."])
  var targetType: Set<String>? = null

  @CommandLine.Option(
      names = ["-o", "--output"],
      scope = CommandLine.ScopeType.LOCAL,
      description =
          [
              "Filepath to write the impacted Bazel targets to. If using depEdgesFile: formatted in json, otherwise: newline separated. If not specified, the output will be written to STDOUT."],
  )
  var outputPath: File? = null

  @CommandLine.Option(
      names = ["-w", "--workspacePath"],
      description =
          [
              "Path to Bazel workspace directory. Required for module change detection."],
      scope = CommandLine.ScopeType.LOCAL,
      required = true,
      converter = [NormalisingPathConverter::class])
  lateinit var workspacePath: Path

  @CommandLine.Option(
      names = ["-b", "--bazelPath"],
      description =
          [
              "Path to Bazel binary. If not specified, the Bazel binary available in PATH will be used."],
      scope = CommandLine.ScopeType.LOCAL,
      defaultValue = CommandLine.Parameters.NULL_VALUE)
  var bazelPath: Path? = null

  @CommandLine.Option(
      names = ["-so", "--bazelStartupOptions"],
      description =
          ["Additional space separated Bazel client startup options used when invoking Bazel"],
      scope = CommandLine.ScopeType.LOCAL,
      converter = [OptionsConverter::class])
  var bazelStartupOptions: List<String> = emptyList()

  @CommandLine.Option(
      names = ["--noBazelrc"],
      negatable = true,
      description = ["Don't use .bazelrc"],
      scope = CommandLine.ScopeType.LOCAL)
  var noBazelrc = false

  @CommandLine.Spec lateinit var spec: CommandLine.Model.CommandSpec

  override fun call(): Int {
    // Stop any existing Koin instance before starting a new one (for E2E tests)
    org.koin.core.context.GlobalContext.stopKoin()

    // Setup modules - include hasher module for module querying
    val resolvedBazelPath = bazelPath ?: java.nio.file.Paths.get("bazel")
    startKoin {
      modules(
        serialisationModule(),
        loggingModule(parent.verbose),
        com.bazel_diff.di.hasherModule(
          workingDirectory = workspacePath,
          bazelPath = resolvedBazelPath,
          contentHashPath = null,
          startupOptions = bazelStartupOptions,
          commandOptions = emptyList(),
          cqueryOptions = emptyList(),
          useCquery = false,
          cqueryExpression = null,
          keepGoing = false,
          trackDeps = false,
          fineGrainedHashExternalRepos = emptySet(),
          fineGrainedHashExternalReposFile = null,
          excludeExternalTargets = false
        )
      )
    }

    return try {
      validate()
      val deserialiser = DeserialiseHashesInteractor()
      val fromData = deserialiser.executeTargetHashWithMetadata(startingHashesJSONPath)
      val toData = deserialiser.executeTargetHashWithMetadata(finalHashesJSONPath)

      val outputWriter =
          BufferedWriter(
              when (val path = outputPath) {
                null -> FileWriter(FileDescriptor.out)
                else -> FileWriter(path)
              })

      try {
        if (depsMappingJSONPath != null) {
          val depsMapping = deserialiser.deserializeDeps(depsMappingJSONPath!!)
          CalculateImpactedTargetsInteractor()
              .executeWithDistances(
                  fromData.hashes,
                  toData.hashes,
                  depsMapping,
                  outputWriter,
                  targetType,
                  fromData.moduleGraphJson,
                  toData.moduleGraphJson)
        } else {
          CalculateImpactedTargetsInteractor()
              .execute(
                  fromData.hashes,
                  toData.hashes,
                  outputWriter,
                  targetType,
                  fromData.moduleGraphJson,
                  toData.moduleGraphJson)
        }
        CommandLine.ExitCode.OK
      } catch (e: IOException) {
        CommandLine.ExitCode.SOFTWARE
      }
    } finally {
      stopKoin()
    }
  }

  private fun validate() {
    if (!startingHashesJSONPath.canRead()) {
      throw CommandLine.ParameterException(
          spec.commandLine(), "Incorrect starting hashes: file doesn't exist or can't be read.")
    }
    if (!finalHashesJSONPath.canRead()) {
      throw CommandLine.ParameterException(
          spec.commandLine(), "Incorrect final hashes: file doesn't exist or can't be read.")
    }
    if (depsMappingJSONPath != null && !depsMappingJSONPath!!.canRead()) {
      throw CommandLine.ParameterException(
          spec.commandLine(), "Incorrect dep edges file: file doesn't exist or can't be read.")
    }
  }
}
