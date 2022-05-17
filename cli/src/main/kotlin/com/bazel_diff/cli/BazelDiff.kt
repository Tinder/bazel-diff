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
