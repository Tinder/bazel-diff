package com.bazel_diff.cli

import com.bazel_diff.cli.converter.NormalisingPathConverter
import com.bazel_diff.cli.converter.OptionsConverter
import com.bazel_diff.di.hasherModule
import com.bazel_diff.di.loggingModule
import com.bazel_diff.di.serialisationModule
import com.bazel_diff.interactor.GenerateHashesInteractor
import org.koin.core.context.startKoin
import org.koin.core.context.stopKoin
import picocli.CommandLine
import java.io.File
import java.nio.file.Path
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
        names = ["--contentHashPath"],
        description = ["Path to content hash json file. It's a map which maps relative file path from workspace path to its content hash. Files in this map will skip content hashing and use provided value"],
        scope = CommandLine.ScopeType.INHERIT,
        required = false
    )
    var contentHashPath: File? = null

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
        validate(contentHashPath=contentHashPath)

        startKoin {
            modules(
                hasherModule(
                    workspacePath,
                    bazelPath,
                    contentHashPath,
                    bazelStartupOptions,
                    bazelCommandOptions,
                    keepGoing,
                ),
                loggingModule(parent.verbose),
                serialisationModule(),
            )
        }

        return when (GenerateHashesInteractor().execute(seedFilepaths, output)) {
            true -> CommandLine.ExitCode.OK
            false -> CommandLine.ExitCode.SOFTWARE
        }.also { stopKoin() }
    }

    private fun validateOutput(output: File?): File {
        return output ?: throw CommandLine.ParameterException(
            spec.commandLine(),
            "No output path specified."
        )
    }

    private fun validate(contentHashPath: File?) {
        contentHashPath?.let {
            if (!it.canRead()) {
                throw CommandLine.ParameterException(
                    spec.commandLine(),
                    "Incorrect contentHashFilePath: file doesn't exist or can't be read."
                )
            }
        }
    }
}
