package com.bazel_diff;

import java.io.BufferedReader;
import java.io.File;
import java.io.IOException;
import java.io.InputStreamReader;
import java.nio.file.Path;
import java.util.List;
import java.util.Set;
import java.util.stream.Collectors;

interface GitClient {
    File getRepoRootDirectory() throws IOException, InterruptedException, GitClientException;
    Set<Path> getModifiedFilepaths(String startRevision, String endRevision) throws IOException, InterruptedException;
    void checkoutRevision(String revision) throws IOException, InterruptedException;
    String getHeadSha() throws GitClientException, IOException, InterruptedException;
    void ensureAllChangesAreCommitted() throws IOException, InterruptedException, GitClientException;
}

class GitClientException extends Exception
{
    GitClientException(String message)
    {
        super(message);
    }
}

class GitClientImpl implements GitClient {
    private Path workingDirectory;

    GitClientImpl(Path workingDirectory) {
        this.workingDirectory = workingDirectory;
    }

    @Override
    public File getRepoRootDirectory() throws IOException, InterruptedException, GitClientException {
        List<String> gitOutput = performGitCommand("git", "rev-parse", "--show-toplevel");
        if (gitOutput.get(0) == null) {
            throw new GitClientException("Unable to determine Git root directory, is this a Git repository");
        }
        return new File(gitOutput.get(0));
    }

    @Override
    public Set<Path> getModifiedFilepaths(String startRevision, String endRevision) throws IOException, InterruptedException {
        List<String> gitOutput = performGitCommand("git", "diff", "--name-only", startRevision, endRevision);
        return gitOutput.stream().map( path -> new File(path).toPath()).collect(Collectors.toSet());
    }

    @Override
    public void checkoutRevision(String revision) throws IOException, InterruptedException {
        performGitCommand("git", "checkout", revision, "--quiet");
    }

    @Override
    public String getHeadSha() throws GitClientException, IOException, InterruptedException {
        List<String> gitOutput = performGitCommand("git", "rev-parse", "HEAD");
        if (gitOutput.get(0) == null) {
            throw new GitClientException("Unable to determine HEAD revision SHA");
        }
        return gitOutput.get(0);
    }

    @Override
    public void ensureAllChangesAreCommitted() throws IOException, InterruptedException, GitClientException {
        List<String> gitOutput = performGitCommand("git", "status", "--porcelain");
        if (gitOutput.size() > 0) {
            throw new GitClientException("There are working changes in the directory, please commit them and try again");
        }
    }

    private List<String> performGitCommand(String... command) throws IOException, InterruptedException {
        ProcessBuilder pb = new ProcessBuilder(
            command
        );
        pb.directory(workingDirectory.toFile());
        Process process = pb.start();
        BufferedReader buffer = new BufferedReader( new InputStreamReader(process.getInputStream()));
        return buffer.lines().collect(Collectors.toList());
    }
}
