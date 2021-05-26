package com.bazel_diff;

import java.nio.file.Path;
import java.nio.file.Files;
import java.io.IOException;

interface FilesClient {
    byte[] readFile(Path path) throws IOException;
}

class FilesClientImp implements FilesClient {
    FilesClientImp() {}

    @Override
    public byte[] readFile(Path path) throws IOException {
        if (path.toFile().exists() && path.toFile().canRead()) {
            return Files.readAllBytes(path);
        }
        return new byte[0];
    }
}
