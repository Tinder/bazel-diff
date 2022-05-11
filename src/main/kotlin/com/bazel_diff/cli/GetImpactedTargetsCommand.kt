package com.bazel_diff.cli

import com.bazel_diff.di.mainModule
import com.bazel_diff.interactor.CalculateImpactedTargetsInteractor
import com.bazel_diff.interactor.DeserialiseHashesInteractor
import org.koin.core.context.startKoin
import picocli.CommandLine
import java.io.File
import java.util.concurrent.Callable

@CommandLine.Command(
    name = "get-impacted-targets",
    description = ["Command-line utility to analyze the state of the bazel build graph"],
)
class GetImpactedTargetsCommand : Callable<Int> {
    @CommandLine.ParentCommand
    private lateinit var parent: BazelDiff

    @CommandLine.Option(
        names = ["-sh", "--startingHashes"],
        scope = CommandLine.ScopeType.LOCAL,
        description = ["The path to the JSON file of target hashes for the initial revision. Run 'generate-hashes' to get this value."],
        required = true,
    )
    lateinit var startingHashesJSONPath: File

    @CommandLine.Option(
        names = ["-fh", "--finalHashes"],
        scope = CommandLine.ScopeType.LOCAL,
        description = ["The path to the JSON file of target hashes for the final revision. Run 'generate-hashes' to get this value."],
        required = true,
    )
    lateinit var finalHashesJSONPath: File

    @CommandLine.Option(
        names = ["-o", "--output"],
        scope = CommandLine.ScopeType.LOCAL,
        description = ["Filepath to write the impacted Bazel targets to, newline separated"],
        required = true,
    )
    lateinit var outputPath: File

    @CommandLine.Spec
    lateinit var spec: CommandLine.Model.CommandSpec

    override fun call(): Int {
        startKoin {
            modules(
                mainModule(
                    parent.workspacePath,
                    parent.bazelPath,
                    parent.bazelStartupOptions,
                    parent.bazelCommandOptions,
                    parent.keepGoing,
                    parent.isVerbose(),
                    parent.debug
                )
            )
        }

        validate()
        val deserialiser = DeserialiseHashesInteractor()
        val from = deserialiser.execute(startingHashesJSONPath)
        val to = deserialiser.execute(finalHashesJSONPath)

        return when (CalculateImpactedTargetsInteractor().execute(from, to, outputPath)) {
            true -> CommandLine.ExitCode.OK
            false -> CommandLine.ExitCode.SOFTWARE
        }
    }

    private fun validate() {
        if (!startingHashesJSONPath.canRead()) {
            throw CommandLine.ParameterException(
                spec.commandLine(),
                "Incorrect starting hashes: file doesn't exist or can't be read."
            )
        }
        if (!finalHashesJSONPath.canRead()) {
            throw CommandLine.ParameterException(
                spec.commandLine(),
                "Incorrect final hashes: file doesn't exist or can't be read."
            )
        }
    }
}
