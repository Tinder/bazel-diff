package com.bazel_diff;

import com.google.devtools.build.lib.query2.proto.proto2api.Build;

import java.io.*;
import java.nio.charset.StandardCharsets;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;
import java.util.Arrays;

interface BazelClient {
    List<BazelTarget> queryAllTargets() throws IOException;
    Map<String, BazelSourceFileTarget> queryAllSourcefileTargets() throws IOException, NoSuchAlgorithmException;
}

class BazelClientImpl implements BazelClient {
    private Path workingDirectory;
    private Path bazelPath;
    private Boolean verbose;
    private Boolean keepGoing;
    private Boolean debug;
    private List<String> startupOptions;
    private List<String> commandOptions;

    BazelClientImpl(
            Path workingDirectory,
            Path bazelPath,
            String startupOptions,
            String commandOptions,
            Boolean keepGoing,
            Boolean verbose,
            Boolean debug
    ) {
        this.workingDirectory = workingDirectory.normalize();
        this.bazelPath = bazelPath;
        this.startupOptions = startupOptions != null ? Arrays.asList(startupOptions.split(" ")): new ArrayList<String>();
        this.commandOptions = commandOptions != null ? Arrays.asList(commandOptions.split(" ")): new ArrayList<String>();
        this.verbose = verbose;
        this.keepGoing = keepGoing;
        this.debug = debug;
    }

    @Override
    public List<BazelTarget> queryAllTargets() throws IOException {
        Instant queryStartTime = Instant.now();
        List<Build.Target> targets = performBazelQuery("'//external:all-targets' + '//...:all-targets'");
        Instant queryEndTime = Instant.now();
        if (verbose) {
            long querySeconds = Duration.between(queryStartTime, queryEndTime).getSeconds();
            System.out.printf("BazelDiff: All targets queried in %d seconds%n", querySeconds);
        }
        return targets.stream().map( target -> new BazelTargetImpl(target)).collect(Collectors.toList());
    }

    @Override
    public Map<String, BazelSourceFileTarget> queryAllSourcefileTargets() throws IOException, NoSuchAlgorithmException {
        Instant queryStartTime = Instant.now();
        List<Build.Target> targets = performBazelQuery("kind('source file', //...:all-targets)");
        Instant queryEndTime = Instant.now();
        Map<String, BazelSourceFileTarget> sourceFileTargets = processBazelSourcefileTargets(targets, true);
        Instant contentHashEndTime = Instant.now();
        if (verbose) {
            long querySeconds = Duration.between(queryStartTime, queryEndTime).getSeconds();
            long contentHashSeconds = Duration.between(queryEndTime, contentHashEndTime).getSeconds();
            System.out.printf("BazelDiff: All source files queried in %d seconds%n", querySeconds);
            System.out.printf("BazelDiff: Content hash calculated in %d seconds%n", contentHashSeconds);
        }
        return sourceFileTargets;
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
                        readSourcefileTargets ? workingDirectory : null,
                        verbose
                );
                sourceTargets.put(sourceFileTarget.getName(), sourceFileTarget);
            }
        }
        return sourceTargets;
    }

    private List<Build.Target> performBazelQuery(String query) throws IOException {
        Path tempFile = Files.createTempFile(null, ".txt");
        Files.write(tempFile, query.getBytes(StandardCharsets.UTF_8));

        List<String> cmd = new ArrayList<String>();
        cmd.add((bazelPath.toString()));
        if (verbose) {
            System.out.println(String.format("Executing Query: %s", query));
        }
        if (debug) {
            cmd.add("--bazelrc=/dev/null");
        }
        cmd.addAll(this.startupOptions);
        cmd.add("query");
        cmd.add("--output");
        cmd.add("streamed_proto");
        cmd.add("--order_output=no");
        if (keepGoing != null && keepGoing) {
            cmd.add("--keep_going");
        }
        cmd.addAll(this.commandOptions);
        cmd.add("--query_file");
        cmd.add(tempFile.toString());

        ProcessBuilder pb = new ProcessBuilder(cmd).directory(workingDirectory.toFile());
        pb.redirectError(ProcessBuilder.Redirect.INHERIT);
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

        return targets;
    }
}
