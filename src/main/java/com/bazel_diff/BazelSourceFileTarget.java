package com.bazel_diff;

import java.security.MessageDigest;

interface BazelSourceFileTarget {
    String getName();
    byte[] getDigest();
}

class BazelSourceFileTargetImpl implements BazelSourceFileTarget {

    private String name;
    private MessageDigest digest;

    BazelSourceFileTargetImpl(String name, MessageDigest digest) {
        this.name = name;
        this.digest = digest;
    }

    @Override
    public String getName() {
        return name;
    }

    @Override
    public byte[] getDigest() {
        return digest.digest();
    }
}
