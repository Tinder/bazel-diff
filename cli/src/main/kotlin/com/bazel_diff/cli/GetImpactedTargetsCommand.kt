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

  @CommandLine.Option(
      names = ["--writeEmptyOutput"],
      negatable = true,
      defaultValue = "true",
      fallbackValue = "true",
      description =
          [
              "If true (default), always write the output file (or stdout) even when no targets " +
                  "are impacted. Pass --no-writeEmptyOutput to suppress the write entirely on " +
                  "an empty impacted set, so CI can branch on file existence instead of file " +
                  "contents (`if [ -f impacted.txt ]; then bazel test --target_pattern_file=...`). " +
                  "Only meaningful when -o/--output is set; with stdout, nothing is written either way."],
      scope = CommandLine.ScopeType.LOCAL)
  var writeEmptyOutput: Boolean = true

  @CommandLine.Option(
      names = ["--excludeExternalTargets"],
      negatable = true,
      description =
          [
              "If true, drop labels starting with '//external:' from the impacted-targets output. " +
                  "These synthetic labels are produced for bzlmod-managed external repos so " +
                  "generate-hashes can detect dep changes, but they are not buildable in " +
                  "bzlmod-only mode (Bazel 8.6.0+ with --enable_workspace=false) and will fail " +
                  "downstream `bazel build`. See https://github.com/Tinder/bazel-diff/issues/326. " +
                  "When unset, defaults to true if Bzlmod is detected (via `bazel mod graph`), " +
                  "false otherwise."],
      scope = CommandLine.ScopeType.LOCAL,
      defaultValue = CommandLine.Parameters.NULL_VALUE)
  var excludeExternalTargets: Boolean? = null

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

      // If the user did not pass --[no-]excludeExternalTargets, default to true when bzlmod is
      // enabled (synthetic //external:* labels are not buildable in bzlmod-only mode — #326),
      // otherwise false to preserve WORKSPACE-mode behavior.
      val resolvedExcludeExternalTargets =
          excludeExternalTargets
              ?: org.koin.java.KoinJavaComponent.get<com.bazel_diff.bazel.BazelModService>(
                      com.bazel_diff.bazel.BazelModService::class.java)
                  .isBzlmodEnabled

      val interactor = CalculateImpactedTargetsInteractor()

      // Compute first so we can decide whether to write at all. The interactor's
      // compute step is pure (no I/O); we only open the output file when we
      // actually have something to write or when --writeEmptyOutput is set (the
      // default). Deferring the open avoids leaving an empty file behind when
      // the user opted out of empty output -- the intended CI ergonomics.
      try {
        val depsMapping = depsMappingJSONPath?.let { deserialiser.deserializeDeps(it) }
        if (depsMapping != null) {
          val computed =
              interactor.computeImpactedTargetsWithDistances(
                  fromData.hashes,
                  toData.hashes,
                  depsMapping,
                  targetType,
                  fromData.moduleGraphJson,
                  toData.moduleGraphJson,
                  resolvedExcludeExternalTargets)
          if (computed.isEmpty() && !writeEmptyOutput && outputPath != null) {
            return CommandLine.ExitCode.OK
          }
          interactor.writeImpactedTargetsWithDistances(openOutputWriter(), computed)
        } else {
          val computed =
              interactor.computeImpactedTargets(
                  fromData.hashes,
                  toData.hashes,
                  targetType,
                  fromData.moduleGraphJson,
                  toData.moduleGraphJson,
                  resolvedExcludeExternalTargets)
          if (computed.isEmpty() && !writeEmptyOutput && outputPath != null) {
            return CommandLine.ExitCode.OK
          }
          interactor.writeImpactedTargets(openOutputWriter(), computed)
        }
        CommandLine.ExitCode.OK
      } catch (e: IOException) {
        CommandLine.ExitCode.SOFTWARE
      }
    } finally {
      stopKoin()
    }
  }

  private fun openOutputWriter(): BufferedWriter =
      BufferedWriter(
          when (val path = outputPath) {
            null -> FileWriter(FileDescriptor.out)
            else -> FileWriter(path)
          })

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
