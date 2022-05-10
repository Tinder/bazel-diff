package com.bazel_diff;

import java.io.IOException;
import java.nio.file.Path;
import java.util.*;

import com.google.common.collect.MapDifference;
import com.google.common.collect.Maps;
import com.google.common.collect.Sets;
import com.google.common.hash.Hasher;
import com.google.common.hash.Hashing;

import java.nio.charset.StandardCharsets;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;
import java.util.stream.Collectors;

interface TargetHashingClient {
    Map<String, String> hashAllBazelTargetsAndSourcefiles(Set<Path> seedFilepaths) throws Exception;

    Set<String> getImpactedTargets(Map<String, String> startHashes, Map<String, String> endHashes) throws IOException;
}

class TargetHashingClientImpl implements TargetHashingClient {
    private BazelClient bazelClient;
    private FilesClient files;
    private static final byte[] HEX_ARRAY = "0123456789abcdef".getBytes(StandardCharsets.US_ASCII);

    TargetHashingClientImpl(BazelClient bazelClient, FilesClient files) {
        this.bazelClient = bazelClient;
        this.files = files;
    }

    @Override
    public Map<String, String> hashAllBazelTargetsAndSourcefiles(Set<Path> seedFilepaths) throws Exception {
        Map<String, BazelSourceFileTarget> bazelSourcefileTargets = bazelClient.queryAllSourcefileTargets();
        List<BazelTarget> allTargets = bazelClient.queryAllTargets();
        return hashAllTargets(createSeedForFilepaths(seedFilepaths), bazelSourcefileTargets, allTargets);
    }

    @Override
    public Set<String> getImpactedTargets(
            Map<String, String> startHashes,
            Map<String, String> endHashes)
            throws IOException {
        /**
         * This call might be faster if end hashes is a sorted map
         */
        MapDifference<String, String> difference = Maps.difference(endHashes, startHashes);
        Set<String> onlyInEnd = difference.entriesOnlyOnLeft().keySet();
        Set<String> changed = difference.entriesDiffering().keySet();

        HashSet<String> result = new HashSet<>();
        result.addAll(onlyInEnd);
        result.addAll(changed);

        return result;
    }

    private byte[] createDigestForTarget(
            BazelTarget target,
            Map<String, BazelRule> allRulesMap,
            Map<String, BazelSourceFileTarget> bazelSourcefileTargets,
            ConcurrentMap<String, byte[]> ruleHashes,
            byte[] seedHash
    ) {
        if (target.hasSourceFile()) {
            String sourceFileName = getNameForTarget(target);
            if (sourceFileName != null) {
                Hasher hasher = Hashing.sha256().newHasher();
                byte[] sourceTargetDigestBytes = getDigestForSourceTargetName(sourceFileName, bazelSourcefileTargets);
                if (sourceTargetDigestBytes != null) {
                    hasher.putBytes(sourceTargetDigestBytes);
                }
                if (seedHash != null) {
                    hasher.putBytes(seedHash);
                }
                return hasher.hash().asBytes().clone();
            }
        }
        if (target.hasGeneratedFile()) {
            byte[] generatingRuleDigest = ruleHashes.get(target.getGeneratingRuleName());
            if (generatingRuleDigest == null) {
                return createDigestForRule(allRulesMap.get(target.getGeneratingRuleName()), allRulesMap, ruleHashes, bazelSourcefileTargets, seedHash);
            }
            return ruleHashes.get(target.getGeneratingRuleName()).clone();
        }
        BazelRule targetRule = target.getRule();
        return createDigestForRule(targetRule, allRulesMap, ruleHashes, bazelSourcefileTargets, seedHash);
    }

    private byte[] createDigestForRule(
            BazelRule rule,
            Map<String, BazelRule> allRulesMap,
            ConcurrentMap<String, byte[]> ruleHashes,
            Map<String, BazelSourceFileTarget> bazelSourcefileTargets,
            byte[] seedHash
    ) {
        byte[] existingByteArray = ruleHashes.get(rule.getName());
        if (existingByteArray != null) {
            return existingByteArray;
        }
        Hasher hasher = Hashing.sha256().newHasher();
        byte[] digest = rule.getDigest();
        hasher.putBytes(digest);
        if (seedHash != null) {
            hasher.putBytes(seedHash);
        }
        for (String ruleInput : rule.getRuleInputList()) {
            hasher.putBytes(ruleInput.getBytes());
            BazelRule inputRule = allRulesMap.get(ruleInput);
            if(inputRule == null && bazelSourcefileTargets.containsKey(ruleInput)) {
                byte[] sourceFileDigest = getDigestForSourceTargetName(ruleInput, bazelSourcefileTargets);
                if (sourceFileDigest != null) {
                    hasher.putBytes(sourceFileDigest);
                }
            } else if (inputRule != null && inputRule.getName() != null && !inputRule.getName().equals(rule.getName())) {
                byte[] ruleInputHash = createDigestForRule(
                        inputRule,
                        allRulesMap,
                        ruleHashes,
                        bazelSourcefileTargets,
                        seedHash
                );
                if (ruleInputHash != null) {
                    hasher.putBytes(ruleInputHash);
                }
            } else {
                System.out.printf("BazelDiff: [Warning] Unable to calculate digest for input %s for rule %s%n", ruleInput, rule.getName());
            }
        }
        byte[] finalHashValue = hasher.hash().asBytes().clone();
        ruleHashes.put(rule.getName(), finalHashValue);
        return finalHashValue;
    }

    private byte[] createSeedForFilepaths(Set<Path> seedFilepaths) throws IOException {
        if (seedFilepaths == null || seedFilepaths.size() == 0) {
            return new byte[0];
        }
        Hasher hasher = Hashing.sha256().newHasher();
        for (Path path : seedFilepaths) {
            hasher.putBytes(this.files.readFile(path));
        }
        return hasher.hash().asBytes().clone();
    }

    private byte[] getDigestForSourceTargetName(
            String sourceTargetName,
            Map<String, BazelSourceFileTarget> bazelSourcefileTargets
    ) {
        BazelSourceFileTarget target = bazelSourcefileTargets.get(sourceTargetName);
        return target != null ? target.getDigest() : null;
    }

    private String convertByteArrayToString(byte[] bytes) {
        byte[] hexChars = new byte[bytes.length * 2];
        for (int j = 0; j < bytes.length; j++) {
            int v = bytes[j] & 0xFF;
            hexChars[j * 2] = HEX_ARRAY[v >>> 4];
            hexChars[j * 2 + 1] = HEX_ARRAY[v & 0x0F];
        }
        return new String(hexChars, StandardCharsets.UTF_8);
    }

    private String getNameForTarget(BazelTarget target) {
        if (target.hasRule()) {
            return target.getRule().getName();
        }
        if (target.hasSourceFile()) {
            return target.getSourceFileName();
        }
        if (target.hasGeneratedFile()) {
            return target.getGeneratedFileName();
        }
        return null;
    }

    private Map<String, String> hashAllTargets(byte[] seedHash, Map<String, BazelSourceFileTarget> bazelSourcefileTargets, List<BazelTarget> allTargets) {
        ConcurrentMap<String, byte[]> ruleHashes = new ConcurrentHashMap<>();
        Map<String, BazelRule> targetToRule = new HashMap<>();

        Set<BazelTarget> targetsToAnalyse = Sets.newHashSet(allTargets);
        while (!targetsToAnalyse.isEmpty()) {
            int initialSize = targetsToAnalyse.size();
            Set<BazelTarget> nextTargets = Sets.newHashSet();
            for (BazelTarget target : targetsToAnalyse) {
                String targetName = getNameForTarget(target);
                if (targetName == null) {
                    System.out.printf("BazelDiff: [Warning] Unknown target name in the build graph. Target type is %s%n", target.getType().name());
                    continue;
                }

                if (target.hasRule()) {
                    targetToRule.put(targetName, target.getRule());
                } else if (target.hasGeneratedFile()) {
                    String generatingRuleName = target.getGeneratingRuleName();
                    BazelRule generatingRule = targetToRule.get(generatingRuleName);
                    if (generatingRule == null) {
                        nextTargets.add(target);
                    } else {
                        targetToRule.put(targetName, generatingRule);
                    }
                } else if(target.hasSourceFile()) {
                    //Source files are hashed separately
                    continue;
                } else {
                    throw new RuntimeException("Unsupported target type in the build graph: " + target.getType().name());
                }
            }
            int newSize = nextTargets.size();
            if(newSize >= initialSize) {
                throw new RuntimeException("Not possible to traverse the build graph");
            }
            targetsToAnalyse = nextTargets;
        }

        return allTargets.parallelStream()
                .map((target) -> {
                    String targetName = getNameForTarget(target);
                    if (targetName != null) {
                        byte[] targetDigest = createDigestForTarget(
                                target,
                                targetToRule,
                                bazelSourcefileTargets,
                                ruleHashes,
                                seedHash
                        );
                        if (targetDigest != null) {
                            return new TargetEntry(targetName, convertByteArrayToString(targetDigest));
                        }
                    }
                    return null;
                })
                .filter((targetEntry -> targetEntry != null))
                .collect(Collectors.toMap(TargetEntry::getKey, TargetEntry::getValue));
    }

    private static class TargetEntry<K extends String, V extends String> {
        private K key;
        private V value;

        public TargetEntry(K key, V value) {
            this.key = key;
            this.value = value;
        }

        public K getKey() {
            return key;
        }

        public V getValue() {
            return value;
        }
    }
}
