package com.bazel_diff;

import java.io.IOException;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.*;

interface TargetHashingClient {
    Map<String, String> hashAllBazelTargets(Set<Path> modifiedFilepaths) throws IOException, NoSuchAlgorithmException;
    Set<String> getImpactedTargets(Map<String, String> startHashes, Map<String, String> endHashes);
    Set<String> getImpactedTestTargets(Map<String, String> startHashes, Map<String, String> endHashes) throws IOException;
}

class TargetHashingClientImpl implements TargetHashingClient {
    private BazelClient bazelClient;

    TargetHashingClientImpl(BazelClient bazelClient) {
        this.bazelClient = bazelClient;
    }

    @Override
    public Map<String, String> hashAllBazelTargets(Set<Path> modifiedFilepaths) throws IOException, NoSuchAlgorithmException {
        Set<BazelSourceFileTarget> bazelSourcefileTargets = bazelClient.convertFilepathsToSourceTargets(modifiedFilepaths);
        List<BazelTarget> allTargets = bazelClient.queryAllTargets();
        Map<String, String> targetHashes = new HashMap<>();
        Map<String, MessageDigest> ruleHashes = new HashMap<>();
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
            MessageDigest targetDigest = createDigestForTarget(
                    target,
                    allRulesMap,
                    bazelSourcefileTargets,
                    ruleHashes
            );
            if (targetDigest != null) {
                targetHashes.put(targetName, digestToString(targetDigest));
            }
        }
        return targetHashes;
    }

    @Override
    public Set<String> getImpactedTargets(Map<String, String> startHashes, Map<String, String> endHashes) {
        Set<String> impactedTargets = new HashSet<>();
        for ( Map.Entry<String,String> entry : endHashes.entrySet()) {
            String startHashValue = startHashes.get(entry.getKey());
            if (startHashValue == null || !startHashValue.equals(entry.getValue())) {
                impactedTargets.add(entry.getKey());
            }
        }
        return impactedTargets;
    }

    @Override
    public Set<String> getImpactedTestTargets(Map<String, String> startHashes, Map<String, String> endHashes) throws IOException {
        Set<String> impactedTargets = getImpactedTargets(startHashes, endHashes);
        return bazelClient.queryForImpactedTestTargets(impactedTargets);
    }

    private MessageDigest createDigestForTarget(
            BazelTarget target,
            Map<String, BazelRule> allRulesMap,
            Set<BazelSourceFileTarget> bazelSourcefileTargets,
            Map<String, MessageDigest> ruleHashes
    ) throws NoSuchAlgorithmException {
        BazelRule targetRule = target.getRule();
        if (target.hasSourceFile()) {
            String sourceFileName = getNameForTarget(target);
            if (sourceFileName != null) {
                MessageDigest digest = MessageDigest.getInstance("SHA-256");
                byte[] sourceTargetDigestBytes = getDigestForSourceTargetName(sourceFileName, bazelSourcefileTargets);
                if (sourceTargetDigestBytes != null) {
                    digest.update(getDigestForSourceTargetName(sourceFileName, bazelSourcefileTargets));
                }
                return digest;
            }
        }
        return createHashForRule(targetRule, allRulesMap, ruleHashes, bazelSourcefileTargets);
    }

    private MessageDigest createHashForRule(
            BazelRule rule,
            Map<String, BazelRule> allRulesMap,
            Map<String, MessageDigest> ruleHashes,
            Set<BazelSourceFileTarget> bazelSourcefileTargets
    ) throws NoSuchAlgorithmException {
        MessageDigest existingMessage = ruleHashes.get(rule.getName());
        if (existingMessage != null) {
            return existingMessage;
        }
        MessageDigest digest = MessageDigest.getInstance("SHA-256");
        digest.update(rule.getDigest());
        for (String ruleInput : rule.getRuleInputList()) {
            digest.update(ruleInput.getBytes());
            BazelRule inputRule = allRulesMap.get(ruleInput);
            byte[] sourceFileDigest = getDigestForSourceTargetName(ruleInput, bazelSourcefileTargets);
            if (inputRule != null) {
                MessageDigest ruleInputDigest = createHashForRule(
                        inputRule,
                        allRulesMap,
                        ruleHashes,
                        bazelSourcefileTargets
                );
                if (ruleInputDigest != null) {
                    digest.update(ruleInputDigest.digest());
                }
            } else if (sourceFileDigest != null) {
                digest.update(sourceFileDigest);
            }
        }
        ruleHashes.put(rule.getName(), digest);
        return digest;
    }

    private byte[] getDigestForSourceTargetName(
            String sourceTargetName,
            Set<BazelSourceFileTarget> bazelSourcefileTargets
    ) {
        for (BazelSourceFileTarget sourceFileTarget : bazelSourcefileTargets) {
            if (sourceFileTarget.getName().equals(sourceTargetName)) {
                return sourceFileTarget.getDigest();
            }
        }
        return null;
    }

    private String digestToString(MessageDigest digest) {
        StringBuilder result = new StringBuilder();
        for (byte aByte : digest.digest()) {
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
}
