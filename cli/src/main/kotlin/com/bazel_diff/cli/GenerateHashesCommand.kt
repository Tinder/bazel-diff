package com.bazel_diff.cli

import com.bazel_diff.cli.converter.CommaSeparatedValueConverter
import com.bazel_diff.cli.converter.NormalisingPathConverter
import com.bazel_diff.cli.converter.OptionsConverter
import com.bazel_diff.di.hasherModule
import com.bazel_diff.di.loggingModule
import com.bazel_diff.di.serialisationModule
import com.bazel_diff.interactor.GenerateHashesInteractor
import java.io.File
import java.nio.file.Path
import java.util.concurrent.Callable
import org.koin.core.context.startKoin
import org.koin.core.context.stopKoin
import picocli.CommandLine

@CommandLine.Command(
    name = "generate-hashes",
    mixinStandardHelpOptions = true,
    description =
        ["Writes to a file the SHA256 hashes for each Bazel Target in the provided workspace."],
    versionProvider = VersionProvider::class)
class GenerateHashesCommand : Callable<Int> {
  @CommandLine.ParentCommand private lateinit var parent: BazelDiff

  @CommandLine.Option(
      names = ["-w", "--workspacePath"],
      description = ["Path to Bazel workspace directory."],
      scope = CommandLine.ScopeType.INHERIT,
      required = true,
      converter = [NormalisingPathConverter::class])
  lateinit var workspacePath: Path

  @CommandLine.Option(
      names = ["-b", "--bazelPath"],
      description =
          [
              "Path to Bazel binary. If not specified, the Bazel binary available in PATH will be used."],
      scope = CommandLine.ScopeType.INHERIT,
      defaultValue = "bazel",
  )
  lateinit var bazelPath: Path

  @CommandLine.Option(
      names = ["--contentHashPath"],
      description =
          [
              "Path to content hash json file. It's a map which maps relative file path from workspace path to its content hash. Files in this map will skip content hashing and use provided value"],
      scope = CommandLine.ScopeType.INHERIT,
      required = false)
  var contentHashPath: File? = null

  @CommandLine.Option(
      names = ["-so", "--bazelStartupOptions"],
      description =
          ["Additional space separated Bazel client startup options used when invoking Bazel"],
      scope = CommandLine.ScopeType.INHERIT,
      converter = [OptionsConverter::class],
  )
  var bazelStartupOptions: List<String> = emptyList()

  @CommandLine.Option(
      names = ["-co", "--bazelCommandOptions"],
      description =
          ["Additional space separated Bazel command options used when invoking `bazel query`"],
      scope = CommandLine.ScopeType.INHERIT,
      converter = [OptionsConverter::class],
  )
  var bazelCommandOptions: List<String> = emptyList()

  @CommandLine.Option(
      names = ["--fineGrainedHashExternalRepos"],
      description =
          [
              "Comma separate list of external repos in which fine-grained hashes are computed for the targets. By default, external repos are treated as an opaque blob. If an external repo is specified here, bazel-diff instead computes the hash for individual targets. For example, one wants to specify `maven` here if they user rules_jvm_external so that individual third party dependency change won't invalidate all targets in the mono repo."],
      scope = CommandLine.ScopeType.INHERIT,
      converter = [CommaSeparatedValueConverter::class],
  )
  var fineGrainedHashExternalRepos: Set<String> = emptySet()

  @CommandLine.Option(
      names = ["--fineGrainedHashExternalReposFile"],
      description =
          [
              "A text file containing a newline separated list of external repos. Similar to --fineGrainedHashExternalRepos but helps you avoid exceeding max arg length. Mutually exclusive with --fineGrainedHashExternalRepos."])
  var fineGrainedHashExternalReposFile: File? = null

  @CommandLine.Option(
      names = ["--useCquery"],
      negatable = true,
      description =
          [
              "If true, use cquery instead of query when generating dependency graphs. Using cquery would yield more accurate build graph at the cost of slower query execution. When this is set, one usually also wants to set `--cqueryCommandOptions` to specify a targeting platform. Note that this flag only works with Bazel 6.2.0 or above because lower versions does not support `--query_file` flag."],
      scope = CommandLine.ScopeType.INHERIT)
  var useCquery = false

  @CommandLine.Option(
      names = ["--includeTargetType"],
      negatable = true,
      description =
          [
              "Whether include target type in the generated JSON or not.\n" +
                  "If false, the generate JSON schema is: {\"<target>\": \"<sha256>\"}\n" +
                  "If true, the generate JSON schema is: {\"<target>\": \"<type>#<sha256>\" }"],
      scope = CommandLine.ScopeType.INHERIT)
  var includeTargetType = false

  @CommandLine.Option(
      names = ["-tt", "--targetType"],
      split = ",",
      scope = CommandLine.ScopeType.LOCAL,
      description =
          [
              "The types of targets to filter. Use comma (,) to separate multiple values, e.g. '--targetType=SourceFile,Rule,GeneratedFile'."])
  var targetType: Set<String>? = null

  @CommandLine.Option(
      names = ["--cqueryCommandOptions"],
      description =
          [
              "Additional space separated Bazel command options used when invoking `bazel cquery`. This flag is has no effect if `--useCquery`is false."],
      scope = CommandLine.ScopeType.INHERIT,
      converter = [OptionsConverter::class],
  )
  var cqueryCommandOptions: List<String> = emptyList()

  @CommandLine.Option(
      names = ["-k", "--keep_going"],
      negatable = true,
      defaultValue = "true",
      fallbackValue = "true",
      description =
          [
              "This flag controls if `bazel query` will be executed with the `--keep_going` flag or not. Disabling this flag allows you to catch configuration issues in your Bazel graph, but may not work for some Bazel setups. Defaults to `true`"],
      scope = CommandLine.ScopeType.INHERIT)
  var keepGoing = false

  @CommandLine.Option(
      names = ["-s", "--seed-filepaths"],
      description =
          [
              "A text file containing a newline separated list of filepaths. Each file in this list will be read and its content will be used as a SHA256 seed when determining affected targets in the build graph. Invalidating any of these files will effectively mark all targets as affected."])
  var seedFilepaths: File? = null

  @CommandLine.Parameters(
      index = "0",
      description =
          [
              "The filepath to write the resulting JSON of dictionary target => SHA-256 values. If not specified, the JSON will be written to STDOUT."],
      defaultValue = CommandLine.Parameters.NULL_VALUE)
  var outputPath: File? = null

  @CommandLine.Option(
      names = ["--ignoredRuleHashingAttributes"],
      description = ["Attributes that should be ignored when hashing rule targets."],
      scope = CommandLine.ScopeType.INHERIT,
      converter = [CommaSeparatedValueConverter::class],
  )
  var ignoredRuleHashingAttributes: Set<String> = emptySet()

  @CommandLine.Option(
      names = ["-d", "--depEdgesFile"],
      description =
          [
              "Path to the file where dependency edges are written to. If not specified, the dependency edges will not be written to a file. Needed for computing build graph distance metrics. See bazel-diff docs for more details about build graph distance metrics."],
      scope = CommandLine.ScopeType.INHERIT,
      defaultValue = CommandLine.Parameters.NULL_VALUE)
  var depsMappingJSONPath: File? = null

  @CommandLine.Option(
      names = ["-m", "--modified-filepaths"],
      description =
          [
              "Experimental: A text file containing a newline separated list of filepaths (relative to the workspace) these filepaths should represent the modified files between the specified revisions and will be used to scope what files are hashed during hash generation."])
  var modifiedFilepaths: File? = null

  @CommandLine.Option(
      names = ["--excludeExternalTargets"],
      negatable = true,
      description =
          [
              "If true, exclude external targets. This must be switched on when using `--enable_workspace=false` Bazel command line option. Defaults to `false`."],
      scope = CommandLine.ScopeType.INHERIT)
  var excludeExternalTargets = false

  @CommandLine.Spec lateinit var spec: CommandLine.Model.CommandSpec

  override fun call(): Int {
    validate(contentHashPath = contentHashPath)

    startKoin {
      modules(
          hasherModule(
              workspacePath,
              bazelPath,
              contentHashPath,
              bazelStartupOptions,
              bazelCommandOptions,
              cqueryCommandOptions,
              useCquery,
              keepGoing,
              depsMappingJSONPath != null,
              fineGrainedHashExternalRepos,
              fineGrainedHashExternalReposFile,
              excludeExternalTargets,
          ),
          loggingModule(parent.verbose),
          serialisationModule(),
      )
    }

    return when (GenerateHashesInteractor()
        .execute(
            seedFilepaths,
            outputPath,
            depsMappingJSONPath,
            ignoredRuleHashingAttributes,
            targetType,
            includeTargetType,
            modifiedFilepaths)) {
      true -> CommandLine.ExitCode.OK
      false -> CommandLine.ExitCode.SOFTWARE
    }.also { stopKoin() }
  }

  private fun validate(contentHashPath: File?) {
    contentHashPath?.let {
      if (!it.canRead()) {
        throw CommandLine.ParameterException(
            spec.commandLine(),
            "Incorrect contentHashFilePath: file doesn't exist or can't be read.")
      }
    }
  }
}
