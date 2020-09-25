package com.bazel_diff;

import com.google.devtools.build.lib.query2.proto.proto2api.Build;

import com.google.common.collect.Iterables;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.stream.Collectors;

interface BazelClient {
    List<BazelTarget> queryAllTargets() throws IOException;
    Set<String> queryForImpactedTestTargets(Set<String> impactedTargets) throws IOException;
    Set<BazelSourceFileTarget> convertFilepathsToSourceTargets(Set<Path> filepaths) throws IOException, NoSuchAlgorithmException;
}

class BazelClientImpl implements BazelClient {
    private Path workingDirectory;
    private Path bazelPath;

    BazelClientImpl(Path workingDirectory, Path bazelPath) {
        this.workingDirectory = workingDirectory.normalize();
        this.bazelPath = bazelPath;
    }

    @Override
    public List<BazelTarget> queryAllTargets() throws IOException {
        List<Build.Target> targets = performBazelQuery("'//external:all-targets' + '//...:all-targets'");
        return targets.stream().map( target -> new BazelTargetImpl(target)).collect(Collectors.toList());
    }

    @Override
    public Set<String> queryForImpactedTestTargets(Set<String> impactedTargets) throws IOException {
        Set<String> impactedTestTargets = new HashSet<>();
        for (List<String> partition : Iterables.partition(impactedTargets, 100)) {
            String targetQuery = partition.stream().collect(Collectors.joining(" + "));
            List<Build.Target> targets = performBazelQuery(String.format("kind(test, rdeps(//..., %s))", targetQuery));
            for (Build.Target target : targets) {
                if (target.hasRule()) {
                    impactedTestTargets.add(target.getRule().getName());
                }
            }
        }
        return impactedTestTargets;
    }

    @Override
    public Set<BazelSourceFileTarget> convertFilepathsToSourceTargets(Set<Path> filepaths) throws IOException, NoSuchAlgorithmException {
        Set<BazelSourceFileTarget> sourceTargets = new HashSet<>();
        for (List<Path> partition : Iterables.partition(filepaths, 1)) {
            String targetQuery = partition
                    .stream()
                    .map(path -> path.toString())
                    .collect(Collectors.joining(" + "));
            List<Build.Target> targets = performBazelQuery(targetQuery);
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
                            digest
                    );
                    sourceTargets.add(sourceFileTarget);
                }
            }
        }
        return sourceTargets;
    }

    private List<Build.Target> performBazelQuery(String query) throws IOException {
        ProcessBuilder pb = new ProcessBuilder(bazelPath.toString(),
                "query",
                query,
                "--output",
                "streamed_proto",
                "--order_output=no",
                "--show_progress=false",
                "--show_loading_progress=false"
        ).directory(workingDirectory.toFile());
        Process process = pb.start();
        ArrayList<Build.Target> targets = new ArrayList<>();
        while (true) {
            Build.Target target = Build.Target.parseDelimitedFrom(process.getInputStream());
            if (target == null) break;  // EOF
            targets.add(target);
        }
        return targets;
    }
}
