package com.bazel_diff;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.*;
import java.util.stream.Collectors;
import com.google.common.primitives.Bytes;

interface TargetHashingClient {
    Map<String, String> hashAllBazelTargetsAndSourcefiles(Set<Path> seedFilepaths) throws IOException, NoSuchAlgorithmException;
    Set<String> getImpactedTargets(Map<String, String> startHashes, Map<String, String> endHashes) throws IOException;
}

class TargetHashingClientImpl implements TargetHashingClient {
    private BazelClient bazelClient;
    private FilesClient files;

    TargetHashingClientImpl(BazelClient bazelClient, FilesClient files) {
        this.bazelClient = bazelClient;
        this.files = files;
    }

    @Override
    public Map<String, String> hashAllBazelTargetsAndSourcefiles(Set<Path> seedFilepaths) throws IOException, NoSuchAlgorithmException {
        Set<BazelSourceFileTarget> bazelSourcefileTargets = bazelClient.queryAllSourcefileTargets();
        return hashAllTargets(createSeedForFilepaths(seedFilepaths), bazelSourcefileTargets);
    }

    @Override
    public Set<String> getImpactedTargets(
        Map<String, String> startHashes,
        Map<String, String> endHashes)
    throws IOException {
        Set<String> impactedTargets = new HashSet<>();
        for (Map.Entry<String,String> entry : endHashes.entrySet()) {
            String startHashValue = startHashes.get(entry.getKey());
            if (startHashValue == null || !startHashValue.equals(entry.getValue())) {
                impactedTargets.add(entry.getKey());
            }
        }
        return impactedTargets;
    }

    private byte[] createDigestForTarget(
            BazelTarget target,
            Map<String, BazelRule> allRulesMap,
            Set<BazelSourceFileTarget> bazelSourcefileTargets,
            Map<String, byte[]> ruleHashes,
            byte[] seedHash
    ) throws NoSuchAlgorithmException {
        BazelRule targetRule = target.getRule();
        if (target.hasSourceFile()) {
            String sourceFileName = getNameForTarget(target);
            if (sourceFileName != null) {
                MessageDigest digest = MessageDigest.getInstance("SHA-256");
                byte[] sourceTargetDigestBytes = getDigestForSourceTargetName(sourceFileName, bazelSourcefileTargets);
                if (sourceTargetDigestBytes != null) {
                    digest.update(sourceTargetDigestBytes);
                }
                if (seedHash != null) {
                    digest.update(seedHash);
                }
                return digest.digest().clone();
            }
        }
        return createDigestForRule(targetRule, allRulesMap, ruleHashes, bazelSourcefileTargets, seedHash);
    }

    private byte[] createDigestForRule(
            BazelRule rule,
            Map<String, BazelRule> allRulesMap,
            Map<String, byte[]> ruleHashes,
            Set<BazelSourceFileTarget> bazelSourcefileTargets,
            byte[] seedHash
    ) throws NoSuchAlgorithmException {
        byte[] existingByteArray = ruleHashes.get(rule.getName());
        if (existingByteArray != null) {
            return existingByteArray;
        }
        MessageDigest digest = MessageDigest.getInstance("SHA-256");
        digest.update(rule.getDigest());
        if (seedHash != null) {
            digest.update(seedHash);
        }
        for (String ruleInput : rule.getRuleInputList()) {
            digest.update(ruleInput.getBytes());
            BazelRule inputRule = allRulesMap.get(ruleInput);
            byte[] sourceFileDigest = getDigestForSourceTargetName(ruleInput, bazelSourcefileTargets);
            if (inputRule != null && !inputRule.getName().equals(rule.getName())) {
                byte[] ruleInputHash = createDigestForRule(
                        inputRule,
                        allRulesMap,
                        ruleHashes,
                        bazelSourcefileTargets,
                        seedHash
                );
                if (ruleInputHash != null) {
                    digest.update(ruleInputHash);
                }
            } else if (sourceFileDigest != null) {
                digest.update(sourceFileDigest);
            }
        }
        byte[] finalHashValue = digest.digest().clone();
        ruleHashes.put(rule.getName(), finalHashValue);
        return finalHashValue;
    }

    private byte[] createSeedForFilepaths(Set<Path> seedFilepaths) throws IOException, NoSuchAlgorithmException {
        if (seedFilepaths == null || seedFilepaths.size() == 0) {
            return new byte[0];
        }
        MessageDigest digest = MessageDigest.getInstance("SHA-256");
        for (Path path: seedFilepaths) {
            digest.update(this.files.readFile(path));
        }
        return digest.digest().clone();
    }

    private byte[] getDigestForSourceTargetName(
            String sourceTargetName,
            Set<BazelSourceFileTarget> bazelSourcefileTargets
    ) throws NoSuchAlgorithmException {
        for (BazelSourceFileTarget sourceFileTarget : bazelSourcefileTargets) {
            if (sourceFileTarget.getName().equals(sourceTargetName)) {
                return sourceFileTarget.getDigest();
            }
        }
        return null;
    }

    private String convertByteArrayToString(byte[] bytes) {
        StringBuilder result = new StringBuilder();
        for (byte aByte : bytes) {
            result.append(String.format("%02x", aByte));
        }
        return result.toString();
    }

    private String getNameForTarget(BazelTarget target) {
        if (target.hasRule()) {
            return target.getRule().getName();
        }
        if (target.hasSourceFile()) {
            return target.getSourceFileName();
        }
        return null;
    }

    private Map<String, String> hashAllTargets(byte[] seedHash, Set<BazelSourceFileTarget> bazelSourcefileTargets) throws IOException, NoSuchAlgorithmException {
        List<BazelTarget> allTargets = bazelClient.queryAllTargets();
        Map<String, String> targetHashes = new HashMap<>();
        Map<String, byte[]> ruleHashes = new HashMap<>();
        Map<String, BazelRule> allRulesMap = new HashMap<>();
        for (BazelTarget target : allTargets) {
            String targetName = getNameForTarget(target);
            if (targetName == null || !target.hasRule()) {
                continue;
            }
            allRulesMap.put(targetName, target.getRule());
        }
        for (BazelTarget target : allTargets) {
            String targetName = getNameForTarget(target);
            if (targetName == null) {
                continue;
            }
            byte[] targetDigest = createDigestForTarget(
                    target,
                    allRulesMap,
                    bazelSourcefileTargets,
                    ruleHashes,
                    seedHash
            );
            if (targetDigest != null) {
                ByteArrayOutputStream outputStream = new ByteArrayOutputStream();
                outputStream.write(targetDigest);
                targetHashes.put(targetName, convertByteArrayToString(outputStream.toByteArray()));
            }
        }
        return targetHashes;
    }
}
