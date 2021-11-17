package com.bazel_diff;

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.nio.file.Path;
import java.nio.file.Files;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.io.IOException;

interface BazelSourceFileTarget {
    String getName();
    byte[] getDigest();
}

class BazelSourceFileTargetImpl implements BazelSourceFileTarget {

    private String name;
    private byte[] digest;

    BazelSourceFileTargetImpl(String name, byte[] digest, Path workingDirectory) throws IOException, NoSuchAlgorithmException {
        this.name = name;
        byte[] data = null;
        if (workingDirectory != null && name.startsWith("//")) {
            String filenameSubstring = name.substring(2);
            String filenamePath = filenameSubstring.replaceFirst(":", "/");
            File sourceFile = new File(workingDirectory.toString(), filenamePath);
            if (sourceFile.isFile() && sourceFile.canRead()) {
                ByteArrayOutputStream outputStream = new ByteArrayOutputStream();
                outputStream.write(Files.readAllBytes(sourceFile.toPath()));
                outputStream.write(digest);
                data = outputStream.toByteArray();
                outputStream.close();
            }
        } else {
            data = digest;
        }
        MessageDigest finalDigest = MessageDigest.getInstance("SHA-256");
        if (data != null) {
            finalDigest.update(data);
        }
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
