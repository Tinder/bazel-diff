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
        name = "modified-filepaths",
        mixinStandardHelpOptions = true,
        description = "Writes to the file the modified filepaths between two revisions.",
        versionProvider = VersionProvider.class
)
class FetchModifiedFilepaths implements Callable<Integer> {

    @Parameters(index = "0", description = "The starting Git revision, e.g. \"HEAD^\"")
    String startingGitRevision;

    @Parameters(index = "1", description = "The final Git revision, e.g. \"HEAD\"")
    String endingGitRevision;

    @Parameters(index = "2", description = "Path that the list of modified files will be written to")
    File outputPath;

    @ParentCommand
    private BazelDiff parent;

    @Override
    public Integer call() {
        GitClient gitClient = new GitClientImpl(parent.workspacePath);
        try {
            gitClient.ensureAllChangesAreCommitted();
        } catch (IOException | InterruptedException e) {
            e.printStackTrace();
            return ExitCode.SOFTWARE;
        } catch (GitClientException e) {
            System.out.println(String.format("There are active changes in '%s', please commit these changes before running modified-filepaths", parent.workspacePath));
            return ExitCode.USAGE;
        }
        try {
            Set<String> modifiedFilepaths = gitClient
                    .getModifiedFilepaths(startingGitRevision, endingGitRevision)
                    .stream()
                    .map( path -> path.toString())
                    .collect(Collectors.toSet());
            FileWriter myWriter = new FileWriter(outputPath);
            myWriter.write(String.join(System.lineSeparator(), modifiedFilepaths));
            myWriter.close();
            return ExitCode.OK;
        } catch (IOException | InterruptedException e) {
            e.printStackTrace();
            return ExitCode.SOFTWARE;
        }
    }
}

@Command(
        name = "generate-hashes",
        mixinStandardHelpOptions = true,
        description = "Writes to a file the SHA256 hashes for each Bazel Target in the provided workspace.",
        versionProvider = VersionProvider.class
)
class GenerateHashes implements Callable<Integer> {

    @ParentCommand
    private BazelDiff parent;

    @Option(names = {"-m", "--modifiedFilepaths"}, description = "The path to a file containing the list of modified filepaths in the workspace, you can use the 'modified-filepaths' command to get this list")
    File modifiedFilepaths;

    @Parameters(index = "0", description = "The filepath to write the resulting JSON of dictionary target => SHA-256 values")
    File outputPath;

    @Override
    public Integer call() {
        GitClient gitClient = new GitClientImpl(parent.workspacePath);
        BazelClient bazelClient = new BazelClientImpl(parent.workspacePath, parent.bazelPath);
        TargetHashingClient hashingClient = new TargetHashingClientImpl(bazelClient);
        try {
            gitClient.ensureAllChangesAreCommitted();
            Set<Path> modifiedFilepathsSet = new HashSet<>();
            if (modifiedFilepaths != null) {
                FileReader fileReader = new FileReader(modifiedFilepaths);
                BufferedReader bufferedReader = new BufferedReader(fileReader);
                modifiedFilepathsSet = bufferedReader
                        .lines()
                        .map( line -> new File(line).toPath())
                        .collect(Collectors.toSet());
            }
            Map<String, String> hashes = hashingClient.hashAllBazelTargets(modifiedFilepathsSet);
            Gson gson = new GsonBuilder().setPrettyPrinting().create();
            FileWriter myWriter = new FileWriter(outputPath);
            myWriter.write(gson.toJson(hashes));
            myWriter.close();
            return ExitCode.OK;
        } catch (GitClientException e) {
            System.out.println(String.format("There are active changes in '%s', please commit these changes before running generate-hashes", parent.workspacePath));
            return ExitCode.USAGE;
        } catch (IOException | InterruptedException | NoSuchAlgorithmException e) {
            e.printStackTrace();
            return ExitCode.SOFTWARE;
        }
    }
}

@Command(
        name = "impacted-tests",
        description = "Write to a file the impacted test targets for the list of Bazel targets in the provided file",
        mixinStandardHelpOptions = true,
        versionProvider = VersionProvider.class
)
class ImpactedTests implements Callable<Integer> {

    @ParentCommand
    private BazelDiff parent;

    @Parameters(index = "0", description = "The filepath to a newline separated list of Bazel targets")
    File impactedBazelTargetsPath;

    @Parameters(index = "1", description = "The filepath to write the impacted test targets to")
    File outputPath;

    @Override
    public Integer call() {
        FileReader fileReader = null;
        try {
            fileReader = new FileReader(impactedBazelTargetsPath);
        } catch (FileNotFoundException e) {
            System.out.println("Unable to read impactedBazelTargetsPath! Exiting");
            return ExitCode.USAGE;
        }
        BufferedReader bufferedReader = new BufferedReader(fileReader);
        BazelClient bazelClient = new BazelClientImpl(parent.workspacePath, parent.bazelPath);
        Set<String> impactedTargets = bufferedReader.lines().collect(Collectors.toSet());
        Set<String> testTargets = null;
        try {
            testTargets = bazelClient.queryForImpactedTestTargets(impactedTargets);
        } catch (IOException e) {
            System.out.println("Unable to query rdeps tests of impacted targets");
            return ExitCode.SOFTWARE;
        }
        try {
            FileWriter myWriter = new FileWriter(outputPath);
            myWriter.write(testTargets.stream().collect(Collectors.joining(System.lineSeparator())));
            myWriter.close();
        } catch (IOException e) {
            System.out.println("Unable to write to outputPath!");
            return ExitCode.USAGE;
        }
        return ExitCode.OK;
    }
}

@Command(
        name = "bazel-diff",
        description = "Writes to a file the impacted targets between two Bazel graph JSON files",
        subcommands = { GenerateHashes.class, FetchModifiedFilepaths.class, ImpactedTests.class },
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

    public Integer call() {
        if (startingHashesJSONPath == null || !startingHashesJSONPath.canRead()) {
            System.out.println("startingHashesJSONPath does not exist! Exiting");
            return ExitCode.USAGE;
        }
        if (finalHashesJSONPath == null || !finalHashesJSONPath.canRead()) {
            System.out.println("finalHashesJSONPath does not exist! Exiting");
            return ExitCode.USAGE;
        }
        GitClient gitClient = new GitClientImpl(workspacePath);
        BazelClient bazelClient = new BazelClientImpl(workspacePath, bazelPath);
        TargetHashingClient hashingClient = new TargetHashingClientImpl(bazelClient);
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
        FileReader startingFileReader = null;
        FileReader finalFileReader = null;
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

    public static void main(String[] args) {
        new CommandLine(new BazelDiff()).execute(args);
    }
}
