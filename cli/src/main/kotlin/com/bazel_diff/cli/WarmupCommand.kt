package com.bazel_diff.cli

import com.bazel_diff.interactor.FingerprintInteractor
import java.io.File
import picocli.CommandLine

/**
 * The Firecracker **record-side entrypoint**. See `docs/firecracker-snapshots.md` §4.1.
 *
 * `warmup` is `generate-hashes` for the base revision plus two metadata side effects:
 * 1. writes the base hashes to a known path (`--base-hashes`, default `/snap/base_hashes.json`),
 * 2. writes the fingerprint file (`--fingerprint-output`, default `/snap/fingerprint.json`).
 *
 * Crucially, it exits `0` **only** once `bazel query` has completed and the server is warm and
 * quiesced — the host watches for this clean exit as the "safe to snapshot" signal.
 *
 * It extends [GenerateHashesCommand] so it inherits the exact same query-affecting flags; warmup is
 * deliberately *generate-hashes plus metadata*, never a divergent query path. That guarantees the
 * base hashes baked into the snapshot are byte-identical to what a cold `generate-hashes` would
 * produce, and that the fingerprint reflects the flags actually used.
 */
@CommandLine.Command(
    name = "warmup",
    mixinStandardHelpOptions = true,
    description =
        [
            "Record-side entrypoint for Firecracker snapshots: runs generate-hashes for the base " +
                "revision, writes base hashes + fingerprint to known paths, and exits 0 only once " +
                "the Bazel server is warm (the host's 'safe to snapshot' signal)."],
    versionProvider = VersionProvider::class)
class WarmupCommand : GenerateHashesCommand() {

  @CommandLine.Option(
      names = ["--base-hashes"],
      description = ["Path to write the base hashes JSON. Default: /snap/base_hashes.json"],
      defaultValue = "/snap/base_hashes.json")
  lateinit var baseHashesPath: File

  @CommandLine.Option(
      names = ["--fingerprint-output"],
      description = ["Path to write the fingerprint JSON. Default: /snap/fingerprint.json"],
      defaultValue = "/snap/fingerprint.json")
  lateinit var fingerprintOutputPath: File

  override fun call(): Int {
    // Route generate-hashes' output to the known base-hashes path.
    outputPath = baseHashesPath
    baseHashesPath.parentFile?.mkdirs()

    val genResult = super.call()
    if (genResult != CommandLine.ExitCode.OK) {
      // Do not write the fingerprint or signal "safe to snapshot" on a failed warmup.
      return genResult
    }

    writeFingerprint()
    return CommandLine.ExitCode.OK
  }

  /**
   * Computes the fingerprint over the current flag set + workspace and writes it to
   * [fingerprintOutputPath]. Split out of [call] so it is unit-testable without the bazel-backed
   * `generate-hashes` run that [call] performs via `super.call()`.
   */
  fun writeFingerprint() {
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

    fingerprintOutputPath.parentFile?.mkdirs()
    fingerprintOutputPath.writeText(renderFingerprintJson(result, inputs.flags) + "\n")
  }
}
