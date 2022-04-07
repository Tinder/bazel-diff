package com.bazel_diff;

import java.io.*;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;

interface BazelSourceFileTarget {
    String getName();
    byte[] getDigest();
}

class BazelSourceFileTargetImpl implements BazelSourceFileTarget {

    private String name;
    private byte[] digest;

    BazelSourceFileTargetImpl(String name, byte[] digest, Path workingDirectory) throws IOException, NoSuchAlgorithmException {
        this.name = name;
        MessageDigest finalDigest = MessageDigest.getInstance("SHA-256");
        if (workingDirectory != null && name.startsWith("//")) {
            String filenameSubstring = name.substring(2);
            String filenamePath = filenameSubstring.replaceFirst(":", "/");
            Path absoluteFilePath = Paths.get(workingDirectory.toString(), filenamePath);
            try (RandomAccessFile sourceFile = new RandomAccessFile(absoluteFilePath.toString(), "r")) {
                FileChannel inChannel = sourceFile.getChannel();
                long fileSize = inChannel.size();
                ByteBuffer buffer = ByteBuffer.allocate((int) fileSize);
                inChannel.read(buffer);
                buffer.flip();
                inChannel.close();
                sourceFile.close();
                finalDigest.update(buffer);
                finalDigest.update(digest);
            } catch (FileNotFoundException e) {}
        } else if (digest != null ) {
            finalDigest.update(digest);
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
