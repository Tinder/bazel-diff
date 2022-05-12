package com.bazel_diff.cli

import com.bazel_diff.di.loggingModule
import com.bazel_diff.di.serialisationModule
import com.bazel_diff.interactor.CalculateImpactedTargetsInteractor
import com.bazel_diff.interactor.DeserialiseHashesInteractor
import org.koin.core.context.startKoin
import org.koin.core.context.stopKoin
import picocli.CommandLine
import java.io.BufferedWriter
import java.io.File
import java.io.FileWriter
import java.io.IOException
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
                serialisationModule(),
                loggingModule(parent.verbose)
            )
        }

        validate()
        val deserialiser = DeserialiseHashesInteractor()
        val from = deserialiser.execute(startingHashesJSONPath)
        val to = deserialiser.execute(finalHashesJSONPath)

        val impactedTargets = CalculateImpactedTargetsInteractor().execute(from, to)

        return try {
            BufferedWriter(FileWriter(outputPath)).use { writer ->
                impactedTargets.forEach {
                    writer.write(it)
                    //Should not be depend on OS
                    writer.write("\n")
                }
            }
            CommandLine.ExitCode.OK
        } catch (e: IOException) {
            CommandLine.ExitCode.SOFTWARE
        }.also { stopKoin() }
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
