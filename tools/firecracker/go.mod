module github.com/Tinder/bazel-diff/tools/firecracker

go 1.21

// Intentionally dependency-free (stdlib only): the Firecracker REST API is
// spoken over a unix socket with net/http, so the orchestrator builds as a
// static CI binary with no module downloads. See README.md.
