package com.bazel_diff;

import java.io.*;
import java.nio.Buffer;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.io.IOException;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;

interface BazelSourceFileTarget {
    String getName();
    byte[] getDigest();
}

class BazelSourceFileTargetImpl implements BazelSourceFileTarget {
    private String name;
    private byte[] digest;

    private void digestLargeFile(MessageDigest finalDigest, FileChannel inChannel) throws IOException {
        int bufferSize = 10240; // 10kb
        ByteBuffer buffer = ByteBuffer.allocate(bufferSize);
        while (inChannel.read(buffer) != -1) {
            ((Buffer)buffer).flip();
            finalDigest.update(buffer);
            buffer.clear();
        }
    }

    private void digestSmallFile(MessageDigest finalDigest, FileChannel inChannel) throws IOException {
        long fileSize = inChannel.size();
        ByteBuffer buffer = ByteBuffer.allocate((int) fileSize);
        inChannel.read(buffer);
        ((Buffer)buffer).flip();
        finalDigest.update(buffer);
    }

    BazelSourceFileTargetImpl(String name, byte[] digest, Path workingDirectory, Boolean verbose)
        throws IOException, NoSuchAlgorithmException {
        this.name = name;
        MessageDigest finalDigest = MessageDigest.getInstance("SHA-256");
        if (workingDirectory != null && name.startsWith("//")) {
            String filenameSubstring = name.substring(2);
            String filenamePath = filenameSubstring.replaceFirst(":", "/");
            Path absoluteFilePath = Paths.get(workingDirectory.toString(), filenamePath);
            try (RandomAccessFile sourceFile = new RandomAccessFile(absoluteFilePath.toString(), "r")) {
                FileChannel inChannel = sourceFile.getChannel();
                if (inChannel.size() > 1048576) { // 1mb
                    digestLargeFile(finalDigest, inChannel);
                } else {
                    digestSmallFile(finalDigest, inChannel);
                }
                sourceFile.close();
                inChannel.close();
            } catch (FileNotFoundException e) {
                if (verbose) {
                    System.out.printf("BazelDiff: [Warning] file %s not found%n", absoluteFilePath);
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
