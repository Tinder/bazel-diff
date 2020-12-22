package com.bazel_diff;

import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;

interface BazelSourceFileTarget {
    String getName();
    byte[] getDigest() throws NoSuchAlgorithmException;
}

class BazelSourceFileTargetImpl implements BazelSourceFileTarget {

    private String name;
    private byte[] digest;

    BazelSourceFileTargetImpl(String name, byte[] digest) {
        this.name = name;
        this.digest = digest;
    }

    @Override
    public String getName() {
        return name;
    }

    @Override
    public byte[] getDigest() throws NoSuchAlgorithmException {
        MessageDigest finalDigest = MessageDigest.getInstance("SHA-256");
        finalDigest.update(digest);
        finalDigest.update(name.getBytes());
        return finalDigest.digest();
    }
}
