package com.bazel_diff.cli

import com.bazel_diff.cli.converter.NormalisingPathConverter
import com.bazel_diff.cli.converter.OptionsConverter
import picocli.CommandLine
import picocli.CommandLine.Model.CommandSpec
import picocli.CommandLine.Spec
import java.nio.file.Path


@CommandLine.Command(
    name = "bazel-diff",
    description = ["Writes to a file the impacted targets between two Bazel graph JSON files"],
    subcommands = [GenerateHashesCommand::class, GetImpactedTargetsCommand::class],
    mixinStandardHelpOptions = true,
    versionProvider = VersionProvider::class,
)
class BazelDiff : Runnable {
    @CommandLine.Option(
        names = ["-w", "--workspacePath"],
        description = ["Path to Bazel workspace directory."],
        scope = CommandLine.ScopeType.INHERIT,
        required = true,
        converter = [NormalisingPathConverter::class]
    )
    lateinit var workspacePath: Path

    @CommandLine.Option(
        names = ["-b", "--bazelPath"],
        description = ["Path to Bazel binary"],
        scope = CommandLine.ScopeType.INHERIT,
        required = true,
    )
    lateinit var bazelPath: Path

    @CommandLine.Option(
        names = ["-so", "--bazelStartupOptions"],
        description = ["Additional space separated Bazel client startup options used when invoking Bazel"],
        scope = CommandLine.ScopeType.INHERIT,
        converter = [OptionsConverter::class],
    )
    var bazelStartupOptions: List<String> = emptyList()

    @CommandLine.Option(
        names = ["-co", "--bazelCommandOptions"],
        description = ["Additional space separated Bazel command options used when invoking Bazel"],
        scope = CommandLine.ScopeType.INHERIT,
        converter = [OptionsConverter::class],
    )
    var bazelCommandOptions: List<String> = emptyList()

    @CommandLine.Option(
        names = ["-k", "--keep_going"],
        negatable = true,
        description = ["This flag controls if `bazel query` will be executed with the `--keep_going` flag or not. Disabling this flag allows you to catch configuration issues in your Bazel graph, but may not work for some Bazel setups. Defaults to `true`"],
        scope = CommandLine.ScopeType.INHERIT
    )
    var keepGoing = true

    @CommandLine.Option(
        names = ["-v", "--verbose"],
        description = ["Display query string, missing files and elapsed time"],
        scope = CommandLine.ScopeType.INHERIT,
    )
    var verbose = false

    @CommandLine.Option(
        names = ["--debug"],
        hidden = true,
        scope = CommandLine.ScopeType.INHERIT,
        defaultValue = "\${sys:DEBUG:-false}"
    )
    var debug = false

    @Spec
    lateinit var spec: CommandSpec

    override fun run() {
        throw CommandLine.ParameterException(spec.commandLine(), "Missing required subcommand")
    }

    fun isVerbose(): Boolean {
        return verbose || debug
    }
}
