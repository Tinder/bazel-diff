package com.bazel_diff;

import com.google.devtools.build.lib.query2.proto.proto2api.Build;

import java.io.*;
import java.nio.charset.StandardCharsets;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;
import java.util.Arrays;

interface BazelClient {
    List<BazelTarget> queryAllTargets() throws IOException, BazelClientQueryError, InterruptedException;
    Map<String, BazelSourceFileTarget> queryAllSourcefileTargets() throws IOException, NoSuchAlgorithmException, BazelClientQueryError, InterruptedException;
}

class BazelClientQueryError extends Exception
{
    BazelClientQueryError(int exitCode, String stdError)
    {
        super(String.format("exitCode %d, stdError: %s", exitCode, stdError));
    }
}

class BazelClientImpl implements BazelClient {
    private Path workingDirectory;
    private Path bazelPath;
    private Boolean verbose;
    private Boolean keepGoing;
    private List<String> startupOptions;
    private List<String> commandOptions;

    BazelClientImpl(
            Path workingDirectory,
            Path bazelPath,
            String startupOptions,
            String commandOptions,
            Boolean verbose,
            Boolean keepGoing
    ) {
        this.workingDirectory = workingDirectory.normalize();
        this.bazelPath = bazelPath;
        this.startupOptions = startupOptions != null ? Arrays.asList(startupOptions.split(" ")): new ArrayList<String>();
        this.commandOptions = commandOptions != null ? Arrays.asList(commandOptions.split(" ")): new ArrayList<String>();
        this.verbose = verbose;
        this.keepGoing = keepGoing;
    }

    @Override
    public List<BazelTarget> queryAllTargets() throws IOException, BazelClientQueryError, InterruptedException {
        List<Build.Target> targets = performBazelQuery("'//external:all-targets' + '//...:all-targets'");
        return targets.stream().map( target -> new BazelTargetImpl(target)).collect(Collectors.toList());
    }

    @Override
    public Map<String, BazelSourceFileTarget> queryAllSourcefileTargets() throws IOException, NoSuchAlgorithmException, BazelClientQueryError, InterruptedException {
        return processBazelSourcefileTargets(performBazelQuery("kind('source file', deps(//...))"), true);
    }

    private Map<String, BazelSourceFileTarget> processBazelSourcefileTargets(List<Build.Target> targets, Boolean readSourcefileTargets) throws IOException, NoSuchAlgorithmException {
        Map<String, BazelSourceFileTarget> sourceTargets = new HashMap<>();
        for (Build.Target target : targets) {
            Build.SourceFile sourceFile = target.getSourceFile();
            if (sourceFile != null) {
                MessageDigest digest = MessageDigest.getInstance("SHA-256");
                digest.update(sourceFile.getNameBytes().toByteArray());
                for (String subinclude : sourceFile.getSubincludeList()) {
                    digest.update(subinclude.getBytes());
                }
                BazelSourceFileTargetImpl sourceFileTarget = new BazelSourceFileTargetImpl(
                        sourceFile.getName(),
                        digest.digest().clone(),
                        readSourcefileTargets ? workingDirectory : null
                );
                sourceTargets.put(sourceFileTarget.getName(), sourceFileTarget);
            }
        }
        return sourceTargets;
    }

    private List<Build.Target> performBazelQuery(String query) throws IOException, BazelClientQueryError, InterruptedException {
        Path tempFile = Files.createTempFile(null, ".txt");
        Files.write(tempFile, query.getBytes(StandardCharsets.UTF_8));

        int[] allowedExitCodes = {0};

        List<String> cmd = new ArrayList<String>();
        cmd.add((bazelPath.toString()));
        if (verbose) {
            System.out.println(String.format("Executing Query: %s", query));
            cmd.add("--bazelrc=/dev/null");
        }
        cmd.addAll(this.startupOptions);
        cmd.add("query");
        cmd.add("--output");
        cmd.add("streamed_proto");
        cmd.add("--order_output=no");
        if (keepGoing != null && keepGoing) {
            // Exit Code 3 is used to express that the query
            // would have failed without the --keep_going flag
            // therefore we must allow this code
            allowedExitCodes = new int[]{0, 3};
            cmd.add("--keep_going");
        }
        cmd.addAll(this.commandOptions);
        cmd.add("--query_file");
        cmd.add(tempFile.toString());

        ProcessBuilder pb = new ProcessBuilder(cmd).directory(workingDirectory.toFile());
        Process process = pb.start();
        ArrayList<Build.Target> targets = new ArrayList<>();

        // Prevent process hang in the case where bazel writes to stderr.
        // See https://stackoverflow.com/questions/3285408/java-processbuilder-resultant-process-hangs
        BufferedReader stdError = new BufferedReader(new InputStreamReader(process.getErrorStream()));
        Thread tStdError = new Thread(new Runnable() {
            String line = null;
            public void run() {
                try {
                    while ((line = stdError.readLine()) != null) {
                        if (verbose) {
                            System.out.println(line);
                        }

                        if(Thread.currentThread().isInterrupted()) {
                            return;
                        }
                    }
                } catch(IOException e) {}
            }
        });
        tStdError.start();

        while (true) {
            Build.Target target = Build.Target.parseDelimitedFrom(process.getInputStream());
            if (target == null) break;  // EOF
            targets.add(target);
        }

        tStdError.interrupt();

        Files.delete(tempFile);

        int exitValue = process.waitFor();

        if (Arrays.stream(allowedExitCodes).noneMatch(value -> exitValue == value)) {
            throw new BazelClientQueryError(
                    exitValue,
                    stdError.lines().collect(Collectors.joining(System.lineSeparator())));
        }

        return targets;
    }
}
