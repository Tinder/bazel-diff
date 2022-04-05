package com.bazel_diff;

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;

interface BazelSourceFileTarget {
    String getName();
    byte[] getDigest();
}

class BazelSourceFileTargetImpl implements BazelSourceFileTarget {
    private String name;
    private byte[] digest;

    BazelSourceFileTargetImpl(String name, byte[] digest, Path workingDirectory)
        throws IOException, NoSuchAlgorithmException {
        this.name = name;
        MessageDigest finalDigest = MessageDigest.getInstance("SHA-256");
        if (workingDirectory != null && name.startsWith("//")) {
            String filenameSubstring = name.substring(2);
            String filenamePath = filenameSubstring.replaceFirst(":", "/");
            File sourceFile = new File(workingDirectory.toString(), filenamePath);
            if (sourceFile.isFile() && sourceFile.canRead()) {
                byte[] buffer = new byte[16384];
                FileInputStream in = new FileInputStream(sourceFile);
                int rc = in.read(buffer);
                while (rc != -1) {
                    finalDigest.update(buffer, 0, rc);
                    rc = in.read(buffer);
                }
            }
        }
        finalDigest.update(digest);
        finalDigest.update(name.getBytes());
        this.digest = finalDigest.digest();
    }

    @Override
    public String getName() {
        return name;
    }

    @Override
    public byte[] getDigest() {
        return this.digest;
    }
}
