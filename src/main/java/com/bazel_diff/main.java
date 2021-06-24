package com.bazel_diff;

import picocli.CommandLine;
import picocli.CommandLine.Option;
import picocli.CommandLine.Parameters;
import picocli.CommandLine.ParentCommand;

import com.google.gson.*;

import java.io.*;
import java.nio.file.Path;
import java.security.NoSuchAlgorithmException;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.Callable;
import java.util.stream.Collectors;

import static picocli.CommandLine.*;

@Command(
        name = "generate-hashes",
        mixinStandardHelpOptions = true,
        description = "Writes to a file the SHA256 hashes for each Bazel Target in the provided workspace.",
        versionProvider = VersionProvider.class
)
class GenerateHashes implements Callable<Integer> {

    @ParentCommand
    private BazelDiff parent;

    @Option(names = {"-s", "--seed-filepaths"}, description = "A text file containing a newline separated list of filepaths, each of these filepaths will be read and used as a seed for all targets.")
    File seedFilepaths;

    @Parameters(index = "0", description = "The filepath to write the resulting JSON of dictionary target => SHA-256 values")
    File outputPath;

    @Override
    public Integer call() {
        BazelClient bazelClient = new BazelClientImpl(parent.workspacePath, parent.bazelPath, parent.bazelStartupOptions, parent.bazelCommandOptions, BazelDiff.isVerbose());
        TargetHashingClient hashingClient = new TargetHashingClientImpl(bazelClient, new FilesClientImp());
        try {
            Set<Path> seedFilepathsSet = new HashSet<>();
            if (seedFilepaths != null) {
                FileReader fileReader = new FileReader(seedFilepaths);
                BufferedReader bufferedReader = new BufferedReader(fileReader);
                seedFilepathsSet = bufferedReader.lines()
                                                .map( line -> new File(line).toPath())
                                                .collect(Collectors.toSet());
            }
            Map<String, String> hashes = hashingClient.hashAllBazelTargetsAndSourcefiles(seedFilepathsSet);
            Gson gson = new GsonBuilder().setPrettyPrinting().create();
            FileWriter myWriter = new FileWriter(outputPath);
            myWriter.write(gson.toJson(hashes));
            myWriter.close();
            return ExitCode.OK;
        } catch (IOException | NoSuchAlgorithmException e) {
            e.printStackTrace();
            return ExitCode.SOFTWARE;
        }
    }
}

@Command(
        name = "bazel-diff",
        description = "Writes to a file the impacted targets between two Bazel graph JSON files",
        subcommands = { GenerateHashes.class },
        mixinStandardHelpOptions = true,
        versionProvider = VersionProvider.class
)
class BazelDiff implements Callable<Integer> {

    @Option(names = {"-w", "--workspacePath"}, description = "Path to Bazel workspace directory.", scope = ScopeType.INHERIT, required = true)
    Path workspacePath;

    @Option(names = {"-b", "--bazelPath"}, description = "Path to Bazel binary", scope = ScopeType.INHERIT, required = true)
    Path bazelPath;

    @Option(names = {"-sh", "--startingHashes"}, scope = ScopeType.LOCAL, description = "The path to the JSON file of target hashes for the initial revision. Run 'generate-hashes' to get this value.")
    File startingHashesJSONPath;

    @Option(names = {"-fh", "--finalHashes"}, scope = ScopeType.LOCAL, description = "The path to the JSON file of target hashes for the final revision. Run 'generate-hashes' to get this value.")
    File finalHashesJSONPath;

    @Option(names = {"-o", "--output"}, scope = ScopeType.LOCAL, description = "Filepath to write the impacted Bazel targets to, newline separated")
    File outputPath;

    @Option(names = {"-so", "--bazelStartupOptions"}, description = "Additional space separated Bazel client startup options used when invoking Bazel", scope = ScopeType.INHERIT)
    String bazelStartupOptions;

    @Option(names = {"-co", "--bazelCommandOptions"}, description = "Additional space separated Bazel command options used when invoking Bazel", scope = ScopeType.INHERIT)
    String bazelCommandOptions;

    @Override
    public Integer call() throws IOException {
        if (startingHashesJSONPath == null || !startingHashesJSONPath.canRead()) {
            System.out.println("startingHashesJSONPath does not exist! Exiting");
            return ExitCode.USAGE;
        }
        if (finalHashesJSONPath == null || !finalHashesJSONPath.canRead()) {
            System.out.println("finalHashesJSONPath does not exist! Exiting");
            return ExitCode.USAGE;
        }
        GitClient gitClient = new GitClientImpl(workspacePath);
        BazelClient bazelClient = new BazelClientImpl(workspacePath, bazelPath, bazelStartupOptions, bazelCommandOptions, BazelDiff.isVerbose());
        TargetHashingClient hashingClient = new TargetHashingClientImpl(bazelClient, new FilesClientImp());
        try {
            gitClient.ensureAllChangesAreCommitted();
        } catch (IOException | InterruptedException e) {
            e.printStackTrace();
            return ExitCode.SOFTWARE;
        } catch (GitClientException e) {
            System.out.println(String.format("There are active changes in '%s', please commit these changes before running bazel-diffs", workspacePath));
            return ExitCode.USAGE;
        }
        Gson gson = new Gson();
        FileReader startingFileReader;
        FileReader finalFileReader;
        try {
            startingFileReader = new FileReader(startingHashesJSONPath);
        } catch (FileNotFoundException e) {
            System.out.println("Starting Hashes JSON filepath does not exist! Exiting");
            return ExitCode.USAGE;
        }
        try {
            finalFileReader = new FileReader(finalHashesJSONPath);
        } catch (FileNotFoundException e) {
            System.out.println("Final Hashes JSON filepath does not exist! Exiting");
            return ExitCode.USAGE;
        }
        Map<String, String > gsonHash = new HashMap<>();
        Map<String, String> startingHashes = gson.fromJson(startingFileReader, gsonHash.getClass());
        Map<String, String> finalHashes = gson.fromJson(finalFileReader, gsonHash.getClass());
        Set<String> impactedTargets = hashingClient.getImpactedTargets(startingHashes, finalHashes);
        try {
            FileWriter myWriter = new FileWriter(outputPath);
            myWriter.write(impactedTargets.stream().collect(Collectors.joining(System.lineSeparator())));
            myWriter.close();
        } catch (IOException e) {
            System.out.println("Unable to write to output filepath! Exiting!");
            return ExitCode.SOFTWARE;
        }
        return ExitCode.OK;
    }

    static Boolean isVerbose() {
        String verboseFlag = System.getProperty("VERBOSE", "false");
        return verboseFlag.equals("true");
    }

    public static void main(String[] args) {
        int exitCode = new CommandLine(new BazelDiff()).execute(args);
        System.exit(exitCode);
    }
}
