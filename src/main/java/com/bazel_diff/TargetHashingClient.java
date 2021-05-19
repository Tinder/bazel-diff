package com.bazel_diff;

import java.io.IOException;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.*;
import java.util.stream.Collectors;

interface TargetHashingClient {
    Map<String, String> hashAllBazelTargets(Set<Path> modifiedFilepaths) throws IOException, NoSuchAlgorithmException;
    Map<String, String> hashAllBazelTargetsAndSourcefiles() throws IOException, NoSuchAlgorithmException;
    Set<String> getImpactedTargets(Map<String, String> startHashes, Map<String, String> endHashes, String avoidQuery, Boolean hashAllTargets) throws IOException;
}

class TargetHashingClientImpl implements TargetHashingClient {
    private BazelClient bazelClient;

    TargetHashingClientImpl(BazelClient bazelClient) {
        this.bazelClient = bazelClient;
    }

    @Override
    public Map<String, String> hashAllBazelTargets(Set<Path> modifiedFilepaths) throws IOException, NoSuchAlgorithmException {
        Set<BazelSourceFileTarget> bazelSourcefileTargets = bazelClient.convertFilepathsToSourceTargets(modifiedFilepaths);
        return hashAllTargets(bazelSourcefileTargets);
    }

    @Override
    public Map<String, String> hashAllBazelTargetsAndSourcefiles() throws IOException, NoSuchAlgorithmException {
        Set<BazelSourceFileTarget> bazelSourcefileTargets = bazelClient.queryAllSourcefileTargets();
        return hashAllTargets(bazelSourcefileTargets);
    }

    @Override
    public Set<String> getImpactedTargets(
        Map<String, String> startHashes,
        Map<String, String> endHashes,
        String avoidQuery,
        Boolean hashAllTargets)
    throws IOException {
        Set<String> impactedTargets = new HashSet<>();
        for (Map.Entry<String,String> entry : endHashes.entrySet()) {
            String startHashValue = startHashes.get(entry.getKey());
            if (startHashValue == null || !startHashValue.equals(entry.getValue())) {
                impactedTargets.add(entry.getKey());
            }
        }
        if (hashAllTargets != null && hashAllTargets && avoidQuery == null) {
            return impactedTargets;
        }
        return bazelClient.queryForImpactedTargets(impactedTargets, avoidQuery);
    }

    private byte[] createDigestForTarget(
            BazelTarget target,
            Map<String, BazelRule> allRulesMap,
            Set<BazelSourceFileTarget> bazelSourcefileTargets,
            Map<String, byte[]> ruleHashes
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
                return digest.digest().clone();
            }
        }
        return createDigestForRule(targetRule, allRulesMap, ruleHashes, bazelSourcefileTargets);
    }

    private byte[] createDigestForRule(
            BazelRule rule,
            Map<String, BazelRule> allRulesMap,
            Map<String, byte[]> ruleHashes,
            Set<BazelSourceFileTarget> bazelSourcefileTargets
    ) throws NoSuchAlgorithmException {
        byte[] existingByteArray = ruleHashes.get(rule.getName());
        if (existingByteArray != null) {
            return existingByteArray;
        }
        MessageDigest digest = MessageDigest.getInstance("SHA-256");
        digest.update(rule.getDigest());
        for (String ruleInput : rule.getRuleInputList()) {
            digest.update(ruleInput.getBytes());
            BazelRule inputRule = allRulesMap.get(ruleInput);
            byte[] sourceFileDigest = getDigestForSourceTargetName(ruleInput, bazelSourcefileTargets);
            if (inputRule != null) {
                byte[] ruleInputHash = createDigestForRule(
                        inputRule,
                        allRulesMap,
                        ruleHashes,
                        bazelSourcefileTargets
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

    private Map<String, String> hashAllTargets(Set<BazelSourceFileTarget> bazelSourcefileTargets) throws IOException, NoSuchAlgorithmException {
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
                    ruleHashes
            );
            if (targetDigest != null) {
                targetHashes.put(targetName, convertByteArrayToString(targetDigest));
            }
        }
        return targetHashes;
    }
}
