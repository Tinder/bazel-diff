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
                    .map(Path::toString)
                    .collect(Collectors.toSet());
            FileWriter myWriter = new FileWriter(outputPath);
            for (String line : modifiedFilepaths) {
                myWriter.write(line);
                myWriter.write(System.lineSeparator());
            }
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

    @ArgGroup(exclusive = true)
    Exclusive exclusive;

    static class Exclusive {
        @Option(names = {"-m", "--modifiedFilepaths"}, description = "The path to a file containing the list of modified filepaths in the workspace, you can use the 'modified-filepaths' command to get this list")
        File modifiedFilepaths;

        @Option(names = {"-a", "--all-sourcefiles"}, description = "Experimental: Hash all sourcefile targets (instead of relying on --modifiedFilepaths), Warning: Performance may degrade from reading all source files")
        Boolean hashAllSourcefiles;
    }

    @Parameters(index = "0", description = "The filepath to write the resulting JSON of dictionary target => SHA-256 values")
    File outputPath;

    @Override
    public Integer call() {
        GitClient gitClient = new GitClientImpl(parent.workspacePath);
        BazelClient bazelClient = new BazelClientImpl(parent.workspacePath, parent.bazelPath, parent.bazelStartupOptions, parent.bazelCommandOptions, BazelDiff.isVerbose());
        TargetHashingClient hashingClient = new TargetHashingClientImpl(bazelClient);
        try {
            gitClient.ensureAllChangesAreCommitted();
            Set<Path> modifiedFilepathsSet = new HashSet<>();
            Map<String, String> hashes = new HashMap<>();
            if (exclusive != null) {
                if (exclusive.modifiedFilepaths != null) {
                    FileReader fileReader = new FileReader(exclusive.modifiedFilepaths);
                    BufferedReader bufferedReader = new BufferedReader(fileReader);
                    modifiedFilepathsSet = bufferedReader
                            .lines()
                            .map( line -> new File(line).toPath())
                            .collect(Collectors.toSet());
                    hashes = hashingClient.hashAllBazelTargets(modifiedFilepathsSet);
                } else if (exclusive.hashAllSourcefiles) {
                    hashes = hashingClient.hashAllBazelTargetsAndSourcefiles();
                }
            } else {
                hashes = hashingClient.hashAllBazelTargets(modifiedFilepathsSet);
            }
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
        name = "bazel-diff",
        description = "Writes to a file the impacted targets between two Bazel graph JSON files",
        subcommands = { GenerateHashes.class, FetchModifiedFilepaths.class },
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

    @Option(names = {"-a", "--all-sourcefiles"}, description = "Experimental: Hash all sourcefile targets (instead of relying on --modifiedFilepaths), Warning: Performance may degrade from reading all source files")
    Boolean hashAllSourcefiles;

    @Option(names = {"-aq", "--avoid-query"}, scope = ScopeType.LOCAL, description = "A Bazel query string, any targets that pass this query will be removed from the returned set of targets")
    String avoidQuery;

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
        Set<String> impactedTargets = hashingClient.getImpactedTargets(startingHashes, finalHashes, avoidQuery, hashAllSourcefiles);
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
