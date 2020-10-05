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
import java.util.Arrays;

interface BazelClient {
    List<BazelTarget> queryAllTargets() throws IOException;
    Set<String> queryForImpactedTargets(Set<String> impactedTargets) throws IOException;
    Set<String> queryForTestTargets(Set<String> targets) throws IOException;
    Set<BazelSourceFileTarget> convertFilepathsToSourceTargets(Set<Path> filepaths) throws IOException, NoSuchAlgorithmException;
}

class BazelClientImpl implements BazelClient {
    private Path workingDirectory;
    private Path bazelPath;
    private List<String> startupOptions;
    private List<String> commandOptions;

    BazelClientImpl(Path workingDirectory, Path bazelPath, String startupOptions, String commandOptions) {
        this.workingDirectory = workingDirectory.normalize();
        this.bazelPath = bazelPath;
        this.startupOptions = startupOptions != null ? Arrays.asList(startupOptions.split(" ")): new ArrayList<String>();
        this.commandOptions = commandOptions != null ? Arrays.asList(commandOptions.split(" ")): new ArrayList<String>();
    }

    @Override
    public List<BazelTarget> queryAllTargets() throws IOException {
        List<Build.Target> targets = performBazelQuery("'//external:all-targets' + '//...:all-targets'");
        return targets.stream().map( target -> new BazelTargetImpl(target)).collect(Collectors.toList());
    }

    @Override
    public Set<String> queryForImpactedTargets(Set<String> impactedTargets) throws IOException {
        Set<String> impactedTestTargets = new HashSet<>();
        for (List<String> partition : Iterables.partition(impactedTargets, 100)) {
            String targetQuery = partition.stream().collect(Collectors.joining(" + "));
            List<Build.Target> targets = performBazelQuery(String.format("rdeps(//..., %s)", targetQuery));
            for (Build.Target target : targets) {
                if (target.hasRule()) {
                    impactedTestTargets.add(target.getRule().getName());
                }
            }
        }
        return impactedTestTargets;
    }

    @Override
    public Set<String> queryForTestTargets(Set<String> targets) throws IOException {
        Set<String> impactedTestTargets = new HashSet<>();
        for (List<String> partition : Iterables.partition(targets, 100)) {
            String targetQuery = partition.stream().collect(Collectors.joining(" + "));
            List<Build.Target> testTargets = performBazelQuery(String.format("kind(test, %s)", targetQuery));
            for (Build.Target target : testTargets) {
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
        List<String> cmd = new ArrayList<String>();
        
        cmd.add((bazelPath.toString()));
        cmd.addAll(this.startupOptions);
        cmd.add("query");
        cmd.add("--output");
        cmd.add("streamed_proto");
        cmd.add("--order_output=no");
        cmd.add("--show_progress=false");
        cmd.add("--show_loading_progress=false");
        cmd.addAll(this.commandOptions);
        cmd.add(query);

        ProcessBuilder pb = new ProcessBuilder(cmd).directory(workingDirectory.toFile());
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
