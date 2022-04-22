package com.bazel_diff;

import java.util.List;

import com.google.common.hash.Hasher;
import com.google.common.hash.Hashing;
import com.google.devtools.build.lib.query2.proto.proto2api.Build;
import java.util.stream.Collectors;

interface BazelRule {
    byte[] getDigest();
    List<String> getRuleInputList();
    String getName();
}

class BazelRuleImpl implements BazelRule {
    private Build.Rule rule;

    BazelRuleImpl(Build.Rule rule) {
        this.rule = rule;
    }

    @Override
    public byte[] getDigest() {
        Hasher hasher = Hashing.sha256().newHasher();
        hasher.putBytes(rule.getRuleClassBytes().toByteArray());
        hasher.putBytes(rule.getNameBytes().toByteArray());
        hasher.putBytes(rule.getSkylarkEnvironmentHashCodeBytes().toByteArray());
        for (Build.Attribute attribute : rule.getAttributeList()) {
            hasher.putBytes(attribute.toByteArray());
        }
        return hasher.hash().asBytes();
    }

    @Override
    public List<String> getRuleInputList() {
        return rule.getRuleInputList()
                   .stream()
                   .map(ruleInput -> transformRuleInput(ruleInput))
                   .collect(Collectors.toList());
    }

    @Override
    public String getName() {
        return rule.getName();
    }

    private String transformRuleInput(String ruleInput) {
        if (ruleInput.startsWith("@")) {
            String[] splitRule = ruleInput.split("//");
            if (splitRule.length == 2) {
                String externalRule = splitRule[0];
                externalRule = externalRule.replaceFirst("@", "");
                return String.format("//external:%s", externalRule);
            }
        }
        return ruleInput;
    }
}
