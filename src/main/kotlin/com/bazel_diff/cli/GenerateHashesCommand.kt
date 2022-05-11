package com.bazel_diff.cli

import com.bazel_diff.di.mainModule
import com.bazel_diff.interactor.GenerateHashesInteractor
import org.koin.core.context.startKoin
import picocli.CommandLine
import java.io.File
import java.util.concurrent.Callable

@CommandLine.Command(
    name = "generate-hashes",
    mixinStandardHelpOptions = true,
    description = ["Writes to a file the SHA256 hashes for each Bazel Target in the provided workspace."],
    versionProvider = VersionProvider::class
)
class GenerateHashesCommand : Callable<Int> {
    @CommandLine.ParentCommand
    private lateinit var parent: BazelDiff

    @CommandLine.Option(
        names = ["-s", "--seed-filepaths"],
        description = ["A text file containing a newline separated list of filepaths, each of these filepaths will be read and used as a seed for all targets."]
    )
    var seedFilepaths: File? = null

    @CommandLine.Parameters(
        index = "0",
        description = ["The filepath to write the resulting JSON of dictionary target => SHA-256 values"]
    )
    var outputPath: File? = null

    @CommandLine.Spec
    lateinit var spec: CommandLine.Model.CommandSpec

    override fun call(): Int {
        val output = validateOutput(outputPath)

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

        return when (GenerateHashesInteractor().execute(seedFilepaths, output)) {
            true -> CommandLine.ExitCode.OK
            false -> CommandLine.ExitCode.SOFTWARE
        }
    }

    private fun validateOutput(output: File?): File {
        return output ?: throw CommandLine.ParameterException(
            spec.commandLine(),
            "No output path specified."
        )
    }
}
