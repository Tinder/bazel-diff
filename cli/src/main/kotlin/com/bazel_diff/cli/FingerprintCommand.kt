package com.bazel_diff.cli

import com.bazel_diff.cli.converter.CommaSeparatedValueConverter
import com.bazel_diff.cli.converter.NormalisingPathConverter
import com.bazel_diff.cli.converter.OptionsConverter
import com.bazel_diff.interactor.FingerprintInteractor
import com.google.gson.GsonBuilder
import java.io.File
import java.nio.file.Path
import java.util.concurrent.Callable
import picocli.CommandLine

/**
 * Computes the Firecracker snapshot cache key ("fingerprint") for the current workspace + flag set
 * and writes it as JSON. Used at record time to tag a snapshot and at consume time to validate a
 * candidate snapshot before trusting it. See `docs/firecracker-snapshots.md` §5.
 *
 * Pure metadata: this command does not run `bazel query` and is cheap. The only external call is
 * `bazel version`.
 */
@CommandLine.Command(
    name = "fingerprint",
    mixinStandardHelpOptions = true,
    description =
        [
            "Computes the snapshot cache key over the inputs that affect the build graph " +
                "(bazel version, MODULE.bazel.lock, .bazelrc, bazel-diff version, flag set) and " +
                "writes it as JSON. Used to decide whether a Firecracker snapshot is safe to consume."],
    versionProvider = VersionProvider::class)
class FingerprintCommand : Callable<Int> {
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

  // --- query-affecting flags (must match the corresponding generate-hashes flags) ---

  @CommandLine.Option(
      names = ["-so", "--bazelStartupOptions"],
      converter = [OptionsConverter::class],
      description =
          ["Bazel client startup options (must match the consuming generate-hashes run)."])
  var bazelStartupOptions: List<String> = emptyList()

  @CommandLine.Option(
      names = ["-co", "--bazelCommandOptions"],
      converter = [OptionsConverter::class],
      description = ["Bazel command options for `bazel query`."])
  var bazelCommandOptions: List<String> = emptyList()

  @CommandLine.Option(
      names = ["--cqueryCommandOptions"],
      converter = [OptionsConverter::class],
      description = ["Bazel command options for `bazel cquery`."])
  var cqueryCommandOptions: List<String> = emptyList()

  @CommandLine.Option(names = ["--useCquery"], negatable = true) var useCquery = false

  @CommandLine.Option(names = ["--cqueryExpression"]) var cqueryExpression: String? = null

  @CommandLine.Option(names = ["--includeTargetType"], negatable = true)
  var includeTargetType = false

  @CommandLine.Option(names = ["-tt", "--targetType"], split = ",")
  var targetType: Set<String>? = null

  @CommandLine.Option(
      names = ["--fineGrainedHashExternalRepos"], converter = [CommaSeparatedValueConverter::class])
  var fineGrainedHashExternalRepos: Set<String> = emptySet()

  @CommandLine.Option(
      names = ["--ignoredRuleHashingAttributes"], converter = [CommaSeparatedValueConverter::class])
  var ignoredRuleHashingAttributes: Set<String> = emptySet()

  @CommandLine.Option(names = ["--excludeExternalTargets"], negatable = true)
  var excludeExternalTargets = false

  @CommandLine.Option(
      names = ["-k", "--keep_going"],
      negatable = true,
      defaultValue = "false",
      fallbackValue = "true")
  var keepGoing = false

  @CommandLine.Option(
      names = ["-o", "--output"],
      description = ["Path to write the fingerprint JSON. Defaults to STDOUT."],
      defaultValue = CommandLine.Parameters.NULL_VALUE)
  var outputPath: File? = null

  override fun call(): Int {
    val flags =
        FingerprintGatherer.canonicalizeFlags(
            bazelStartupOptions = bazelStartupOptions,
            bazelCommandOptions = bazelCommandOptions,
            cqueryCommandOptions = cqueryCommandOptions,
            useCquery = useCquery,
            cqueryExpression = cqueryExpression,
            includeTargetType = includeTargetType,
            targetType = targetType,
            fineGrainedHashExternalRepos = fineGrainedHashExternalRepos,
            ignoredRuleHashingAttributes = ignoredRuleHashingAttributes,
            excludeExternalTargets = excludeExternalTargets,
            keepGoing = keepGoing,
        )
    val inputs =
        FingerprintGatherer.gather(
            workspacePath = workspacePath,
            bazelPath = bazelPath,
            bazelDiffVersion = VersionProvider().version.firstOrNull() ?: "unknown",
            flags = flags,
        )

    val result = FingerprintInteractor().compute(inputs)
    val json = renderFingerprintJson(result, inputs.flags)

    val out = outputPath
    if (out != null && out.path != "-") {
      out.writeText(json + "\n")
    } else {
      println(json)
    }
    return CommandLine.ExitCode.OK
  }
}

/** Renders the fingerprint result + flag set as pretty JSON. Shared with [WarmupCommand]. */
internal fun renderFingerprintJson(
    result: com.bazel_diff.interactor.FingerprintResult,
    flags: Map<String, String>,
): String {
  val gson = GsonBuilder().disableHtmlEscaping().setPrettyPrinting().create()
  return gson.toJson(
      linkedMapOf(
          "fingerprint" to result.fingerprint,
          "components" to result.components,
          "flags" to flags.toSortedMap(),
      ))
}
