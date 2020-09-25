package com.bazel_diff;

import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.List;
import com.google.devtools.build.lib.query2.proto.proto2api.Build;

interface BazelRule {
    byte[] getDigest() throws NoSuchAlgorithmException;
    List<String> getRuleInputList();
    String getName();
}

class BazelRuleImpl implements BazelRule {
    private Build.Rule rule;

    BazelRuleImpl(Build.Rule rule) {
        this.rule = rule;
    }

    @Override
    public byte[] getDigest() throws NoSuchAlgorithmException {
        MessageDigest digest = MessageDigest.getInstance("SHA-256");
        digest.update(rule.getRuleClassBytes().toByteArray());
        digest.update(rule.getNameBytes().toByteArray());
        digest.update(rule.getSkylarkEnvironmentHashCodeBytes().toByteArray());
        for (Build.Attribute attribute : rule.getAttributeList()) {
            digest.update(attribute.toByteArray());
        }
        return digest.digest();
    }

    @Override
    public List<String> getRuleInputList() {
        return rule.getRuleInputList();
    }

    @Override
    public String getName() {
        return rule.getName();
    }
}