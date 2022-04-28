package com.bazel_diff;

import com.google.common.hash.Hasher;
import com.google.common.hash.Hashing;

import java.io.*;
import java.nio.ByteBuffer;
import java.nio.file.Path;
import java.nio.file.Paths;

interface BazelSourceFileTarget {
    String getName();
    byte[] getDigest();
}

class BazelSourceFileTargetImpl implements BazelSourceFileTarget {
    private String name;
    private byte[] digest;
    static private ByteBufferPool pool = new ByteBufferPool(1024, 10240); //10kb

    private void digest(Hasher finalDigest, InputStream stream) throws Exception {
        ByteBuffer buffer = pool.borrow();
        byte[] array = buffer.array(); //Available for non-direct buffers
        Integer length = 0;
        while (true) {
            if (!((length = stream.read(array)) != -1)) break;
            buffer.flip();
            finalDigest.putBytes(array, 0, length);
            buffer.clear();
        }
        pool.recycle(buffer);
    }

    BazelSourceFileTargetImpl(String name, byte[] digest, Path workingDirectory, Boolean verbose) throws Exception {
        this.name = name;
        Hasher hasher = Hashing.sha256().newHasher();
        if (workingDirectory != null && name.startsWith("//")) {
            String filenameSubstring = name.substring(2);
            String filenamePath = filenameSubstring.replaceFirst(":", "/");
            Path absoluteFilePath = Paths.get(workingDirectory.toString(), filenamePath);

            try (InputStream stream = new BufferedInputStream(new FileInputStream(absoluteFilePath.toString()))) {
                digest(hasher, stream);
            } catch (FileNotFoundException e) {
                if (verbose) {
                    System.out.printf("BazelDiff: [Warning] file %s not found%n", absoluteFilePath);
                }
            }
        }
        hasher.putBytes(digest);
        hasher.putBytes(name.getBytes());
        this.digest = hasher.hash().asBytes();
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
