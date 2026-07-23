# bazel-diff

[![Build status](https://github.com/Tinder/bazel-diff/actions/workflows/ci.yaml/badge.svg?branch=master)](https://github.com/Tinder/bazel-diff/actions/workflows/ci.yaml)
[![Coverage](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Tinder/bazel-diff/master/coverage.json)](https://github.com/Tinder/bazel-diff/actions/workflows/ci.yaml)

`bazel-diff` is a command line tool for Bazel projects that allows users to determine the exact affected set of impacted targets between two Git revisions. Using this set, users can test or build the exact modified set of targets.

`bazel-diff` offers several key advantages over rolling your own target diffing solution

1. `bazel-diff` is designed for very large Bazel projects. We use Java Protobuf's `parseDelimitedFrom` method alongside Bazel Query's `streamed_proto` output option. These two together allow you to parse Gigabyte or larger protobuf messages. We have tested it with projects containing tens of thousands of targets.
2. We avoid usage of large command line query lists when interacting with Bazel, [issue here](https://github.com/bazelbuild/bazel/issues/8609). When you interact with Bazel with thousands of query parameters you can reach an upper maximum limit, seeing this error `bash: /usr/local/bin/bazel: Argument list too long`. `bazel-diff` is smart enough to avoid these errors.
3. `bazel-diff` has been tested with file renames, deletions, and modifications. Works on `bzl` files, `WORKSPACE` files, `BUILD` files and regular files

Track the feature request for target diffing in Bazel [here](https://github.com/bazelbuild/bazel/issues/7962)

This approach was inspired by the [following BazelConf talk](https://www.youtube.com/watch?v=9Dk7mtIm7_A) by Benjamin Peterson.

> There are simpler and faster ways to approximate the affected set of targets.
> However an incorrect solution can result in a system you can't trust,
> because tests could be broken at a commit where you didn't select to run them.
> Then you can't rely on green-to-red (or red-to-green) transitions and
> lose much of the value from your CI system as breakages can be discovered
> later on unrelated commits.

## Prerequisites

* Git
* Bazel 3.3.0 or higher
* Java 8 JDK or higher (Bazel requires this)

## Getting Started

To start using `bazel-diff` immediately, simply clone down the repo and then run the example shell script:

```terminal
git clone https://github.com/Tinder/bazel-diff.git
cd bazel-diff
./bazel-diff-example.sh WORKSPACE_PATH BAZEL_PATH START_GIT_REVISION END_GIT_REVISION
```

Here is a breakdown of those arguments:

* `WORKSPACE_PATH`: Path to directory containing your `WORKSPACE` file in your Bazel project.
* `BAZEL_PATH`: Path to your Bazel executable
* `START_GIT_REVISION`: Starting Git Branch or SHA for your desired commit range
* `END_GIT_REVISION`: Final Git Branch or SHA for your desired commit range

You can see the example shell script in action below:

![Demo](demo.gif)

Open `bazel-diff-example.sh` to see how this is implemented. This is purely an example use-case, but it is a great starting point to using `bazel-diff`.

## With Aspect CLI

Aspect's Extension Language (AXL) allows the shell script above to be expressed in Starlark, and exposed as an `impacted` command on your terminal.

See https://github.com/aspect-extensions/impacted

## How it works

`bazel-diff` works as follows

* The previous revision is checked out, then we run `generate-hashes`. This gives us the hashmap representation for the entire Bazel graph, then we write this JSON to a file.

* Next we checkout the initial revision, then we run `generate-hashes` and write that JSON to a file. Now we have our final hashmap representation for the Bazel graph.

* We run `bazel-diff` on the starting and final JSON hash filepaths to get our impacted set of targets. This impacted set of targets is written to a file.

## Build Graph Distance Metrics

`bazel-diff` can optionally compute build graph distance metrics between two revisions. This is
useful for understanding the impact of a change on the build graph. Directly impacted targets are
targets that have had their rule attributes or source file dependencies changed. Indirectly impacted
targets are that are impacted only due to a change in one of their target dependencies.

For each target, the following metrics are computed:

* `target_distance`: The number of dependency hops that it takes to get from an impacted target to a directly impacted target.
* `package_distance`: The number of dependency hops that cross a package boundary to get from an impacted target to a directly impacted target.

Build graph distance metrics can be used by downstream tools to power features such as:

* Only running sanitizers on impacted tests that are in the same package as a directly impacted target.
* Only running large-sized tests that are within a few package hops of a directly impacted target.
* Only running computationally expensive jobs when an impacted target is within a certain distance of a directly impacted target.

To enable this feature, you must generate a dependency mapping on your final revision when computing hashes, then pass it into the `get-impacted-targets` command.

```bash
git checkout BASE_REV
bazel-diff generate-hashes -w /path/to/workspace -b bazel starting_hashes.json

git checkout FINAL_REV
bazel-diff generate-hashes -w /path/to/workspace -b bazel --depEdgesFile deps.json final_hashes.json

bazel-diff get-impacted-targets -w /path/to/workspace -b bazel -sh starting_hashes.json -fh final_hashes.json --depEdgesFile deps.json -o impacted_targets.json
```

This will produce an impacted targets json list with target label, target distance, and package distance:

```text
[
  {"label": "//foo:bar", "targetDistance": 0, "packageDistance": 0},
  {"label": "//foo:baz", "targetDistance": 1, "packageDistance": 0},
  {"label": "//bar:qux", "targetDistance": 1, "packageDistance": 1}
]
```

## Query Service (experimental)

Instead of running `generate-hashes` from scratch on every CI invocation, you can run `bazel-diff` as
a long-running HTTP service that answers affectedness queries between two git revisions and caches
the generated hashes per commit SHA. This is the "bazel-diff as a service" model described in the
[BazelCon talk](https://youtu.be/9Dk7mtIm7_A?t=1875) that inspired this repo (see
[issue #29](https://github.com/Tinder/bazel-diff/issues/29)).

Start the service against a dedicated git clone of your workspace:

```bash
bazel-diff serve \
  --workspacePath /path/to/workspace-clone \
  --bazelPath bazel \
  --cacheDir /var/cache/bazel-diff \
  --port 8080
```

On startup the service performs an initial `git fetch` and only then reports healthy. For each
request it resolves the `from`/`to` revisions, generates (and caches, keyed by commit SHA) the hashes
for each, and reuses the exact same affectedness logic as `get-impacted-targets`.

Endpoints:

* `GET /health` — returns `200 OK` once the initial fetch has completed, `503` otherwise. A load
  balancer should use this to route only to ready instances. If a fatal git error occurs at startup
  the instance "lame-ducks" itself by continuing to report `503` so the load balancer removes it.
* `GET /impacted_targets?from=<rev>&to=<rev>` — returns the impacted targets as JSON. The optional
  `targetType` parameter (e.g. `&targetType=Rule,SourceFile`) filters by target type.

```bash
curl 'http://localhost:8080/impacted_targets?from=main&to=my-feature-branch'
```

```json
{
  "from": "9a1c0e2…",
  "to": "3f7b8d4…",
  "impactedTargets": ["//foo:bar", "//foo:baz"]
}
```

* `POST /impacted_targets` — the same query as a JSON body, which additionally accepts a
  `modifiedFilepaths` list to speed up cold hashing on large repositories. Body fields: `from` and
  `to` (required), `targetType` (optional array), and `modifiedFilepaths` (optional array of
  workspace-relative paths that changed between the two revisions, e.g. from
  `git diff --name-only <from> <to>`). When `modifiedFilepaths` is present the server reads and
  hashes the *content* of only those files on **both** revisions and treats every other source file
  as unchanged, turning an O(all source files) content read into O(changed files) — the same
  optimization as `generate-hashes --modified-filepaths`. The list must be a **superset** of what
  actually changed: a truly-changed file left off it is content-skipped on both sides and its
  impacted targets are missed (hence experimental). Omit it (or send `[]`) for the full-content hash,
  identical to the GET form. `POST /impacted_targets_with_distances` accepts the same body.

```bash
curl -X POST http://localhost:8080/impacted_targets \
  -H 'Content-Type: application/json' \
  -d '{"from":"main","to":"my-feature-branch","modifiedFilepaths":["foo/BUILD.bazel","foo/bar.py"]}'
```

* `GET /impacted_targets_with_distances?from=<rev>&to=<rev>` — like `/impacted_targets`, but each
  impacted target is annotated with its build-graph distance metrics: `targetDistance` (the number of
  dependency hops to the nearest directly-changed target) and `packageDistance` (how many of those
  hops cross a package boundary). Directly-changed targets sit at distance `0`. Requires the server
  to have been started with `--trackDeps` (see below); otherwise this endpoint returns `400`. The
  same optional `targetType` filter applies.

```bash
curl 'http://localhost:8080/impacted_targets_with_distances?from=main&to=my-feature-branch'
```

```json
{
  "from": "9a1c0e2…",
  "to": "3f7b8d4…",
  "impactedTargets": [
    {"label": "//foo:bar", "targetDistance": 0, "packageDistance": 0},
    {"label": "//foo:baz", "targetDistance": 1, "packageDistance": 1}
  ]
}
```

* `GET /metrics` — returns a JSON snapshot of the instance so callers and monitoring can see its
  identity, liveness, and cache size usage without scraping logs. Unlike the query endpoints it is
  never gated on readiness, so it still responds on an un-ready or lame-ducked instance (the `ready`
  field reports the current state). The `cache` size fields are populated for the local-disk backend
  and are `null` for a backend whose size is not cheaply knowable in-process.

```bash
curl 'http://localhost:8080/metrics'
```

```json
{
  "version": "31.4.0",
  "uptimeSeconds": 3600,
  "ready": true,
  "gitEngine": "subprocess",
  "trackDeps": false,
  "cache": {"directory": "/var/cache/bazel-diff", "remote": "s3://my-bucket/bazel-diff/", "entries": 128, "sizeBytes": 4823913, "sizeHuman": "4.6 MB"},
  "jvm": {"usedBytes": 123456789, "maxBytes": 2147483648}
}
```

### Shared S3 cache for multi-instance deployments

A single instance caches hashes on local disk only. When you run several replicas behind a load
balancer (e.g. a Kubernetes Deployment behind a Service, with the readiness probe on `/health`),
give them a shared S3 cache tier so a revision is cold-hashed once fleet-wide instead of once per
pod:

```bash
bazel-diff serve \
  --workspacePath /path/to/workspace-clone \
  --cacheDir /var/cache/bazel-diff \
  --s3Bucket my-hash-cache-bucket \
  --s3Prefix bazel-diff/my-repo
```

With `--s3Bucket` set the cache becomes two-tiered: reads check local disk first and fall back to
the bucket (backfilling local disk on a hit), and every generated entry is published to both, so
any replica can serve a revision another replica already hashed. Credentials and region resolve
through the standard AWS default provider chains (environment variables, profile, IRSA web
identity on EKS, IMDS), or pin the region with `--s3Region`. `--s3Endpoint` plus
`--s3ForcePathStyle` point the client at an S3-compatible store (MinIO, LocalStack) for local
testing.

S3 errors never fail a request: a failed read is treated as a cache miss (the revision is
regenerated — slower, but correct) and a failed write leaves the entry local-only, so an S3 outage
degrades throughput rather than availability. Concurrent replicas racing to hash the same
revision are also harmless — entries are deterministic per key, so last-write-wins over identical
content. The `--cacheMax*` pruning flags bound the *local* tier only; bound the bucket with an S3
lifecycle policy instead.

Notes and current limitations:

* Distance metrics (`/impacted_targets_with_distances`) require the dependency-edge graph, which is
  only tracked when the server is started with `--trackDeps`. Tracking deps grows each cached hash
  entry, so it is opt-in. The flag is folded into the cache key, so enabling or disabling it never
  reuses a previously cached entry of the other kind. This mirrors the `generate-hashes --depEdgesFile`
  / `get-impacted-targets --depEdgesFile` flow used by the CLI.

* The service checks out revisions inside `--workspacePath`, so point it at a dedicated clone, not a
  working tree you edit. All workspace-mutating work (git checkout + `bazel query`) is serialized,
  so a single instance answers one cold query at a time; the per-SHA cache absorbs the rest.
* Git operations (fetch and checkout) shell out to the `git` binary at `--gitPath` (default `git`
  on the `PATH`), so a `git` binary must be available on the host. The working tree is checked out
  on disk for `bazel query` to read. Because native git performs every fetch, all clone shapes are
  supported -- including shallow (`--depth`) and partial (`--filter=blob:none`) clones, whose thin
  packs are delta-compressed against objects the clone does not have.
* Hashes are cached on local disk via `--cacheDir` and survive restarts. Left unbounded the cache
  grows by one entry per distinct commit SHA queried, so a long-running server can bound it with any
  combination of `--cacheMaxAge` (expire entries not read or written within a window, e.g. `7d`),
  `--cacheMaxEntries`, and `--cacheMaxSize` (e.g. `10GB`, `500MB`, or a bare byte count). A background
  sweeper enforces the limits once at startup and then every `--cachePruneInterval` (default `1h`),
  evicting least-recently-used entries first — a cache hit refreshes an entry's recency, so revisions
  under active query are not expired out from under live traffic. With no `--cacheMax*` flag set the
  cache is never pruned (the previous behavior). The `--cacheMax*` flags always bound the local-disk
  tier only; the shared S3 tier (see above) manages its own retention via a bucket lifecycle policy.
* Query-affecting flags (`--useCquery`, `--fineGrainedHashExternalRepos`, etc.) mirror
  `generate-hashes`, and are folded into the cache key so a server started with different flags never
  serves another configuration's cached hashes.
* `modifiedFilepaths` (POST only) is scoped per request, not a server flag. A scoped hash of a
  revision is only comparable to another revision hashed with the *same* set, so cached scoped
  entries are keyed by `<sha>.<fingerprint>.<digest-of-the-set>` — never mixed with, or served in
  place of, the full-content `<sha>.<fingerprint>` entry. The trade-off: on the scoped path the
  shared-base full-hash cache is not reused (each distinct changed-set re-hashes the base), but each
  such hash is cheaper because it skips reading unchanged files. The extra entries are bounded by the
  same LRU `--cacheMax*` pruning as everything else.
* Containerization and multi-instance deployment manifests are not yet included; the shared S3
  cache tier above is the building block for running replicas behind a load balancer.

<!-- BEGIN_SECTION: cli-help -->
<!-- END_SECTION: cli-help -->

### What does the SHA256 value of `generate-hashes` represent?

`generate-hashes` is a canonical SHA256 value representing all attributes and inputs into a target. These inputs
are the summation of the rule implementation hash, the SHA256 value
for every attribute of the rule and then the summation of the SHA256 value for
all `rule_inputs` using the same exact algorithm. For source_file inputs the
content of the file are converted into a SHA256 value.

## Installing

### Integrate into your project (recommended)

First, add the following snippet to your project:

#### Bzlmod snippet

```bazel
bazel_dep(name = "bazel-diff", version = "{{BAZEL_DIFF_VERSION}}")
```

You can now run the tool with:

```terminal
bazel run @bazel-diff//cli:bazel-diff
```

#### WORKSPACE snippet

```bazel
http_jar = use_repo_rule("@bazel_tools//tools/build_defs/repo:http.bzl", "http_jar")
http_jar(
    name = "bazel-diff",
    urls = [
        "https://github.com/Tinder/bazel-diff/releases/download/7.0.0/bazel-diff_deploy.jar"
    ],
    sha256 = "0b9e32f9c20e570846b083743fe967ae54d13e2a1f7364983e0a7792979442be",
)
```

Second, add in your root `BUILD.bazel` file:

```bazel
load("@rules_java//java:defs.bzl", "java_binary")

java_binary(
    name = "bazel-diff",
    main_class = "com.bazel_diff.Main",
    runtime_deps = ["@bazel-diff//jar"],
)
```

That's it! You can now run the tool with:

```terminal
bazel run //:bazel-diff
```

> Note, in releases prior to 2.0.0 the value for the `main_class` attribute is just `BazelDiff`

### Run Via JAR Release

```terminal
curl -Lo bazel-diff.jar https://github.com/Tinder/bazel-diff/releases/latest/download/bazel-diff_deploy.jar
java -jar bazel-diff.jar -h
```

### Build from Source

After cloning down the repo, you are good to go, Bazel will handle the rest

To run the project

```terminal
bazel run :bazel-diff -- bazel-diff -h
```

#### Debugging (when running from source)

To run `bazel-diff` with debug logging, run your commands with the `verbose` config like so:

```terminal
bazel run :bazel-diff --config=verbose -- bazel-diff -h
```

### Build your own deployable JAR

```terminal
bazel build //cli:bazel-diff_deploy.jar
java -jar bazel-bin/cli/bazel-diff_deploy.jar # This JAR can be run anywhere
```

### Build from source in your Bazel Project

Add the following to your `WORKSPACE` file to add the external repositories, replacing the `RELEASE_ARCHIVE_URL` with the archive url of the bazel-diff release you wish to depend on:

```bazel
load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")

http_archive(
  name = "bazel-diff",
  urls = [
        "RELEASE_ARCHIVE_URL",
    ],
    sha256 = "UPDATE_ME",
    strip_prefix = "UPDATE_ME"
)

load("@bazel-diff//:repositories.bzl", "bazel_diff_dependencies")

bazel_diff_dependencies()

load("@rules_jvm_external//:defs.bzl", "maven_install")
load("@bazel-diff//:artifacts.bzl", "BAZEL_DIFF_MAVEN_ARTIFACTS")

maven_install(
    name = "bazel_diff_maven",
    artifacts = BAZEL_DIFF_MAVEN_ARTIFACTS,
    repositories = [
        "http://uk.maven.org/maven2",
        "https://jcenter.bintray.com/",
    ],
)
```

Now you can simply run `bazel-diff` from your project:

```terminal
bazel run @bazel-diff//cli:bazel-diff -- bazel-diff -h
```

## Contributors

<!-- BEGIN_SECTION: contributors -->
<!-- END_SECTION: contributors -->

## Learn More

Take a look at the following bazelcon talks to learn more about `bazel-diff`:

* [BazelCon 2023: Improving CI efficiency with Bazel querying and bazel-diff](https://www.youtube.com/watch?v=QYAbmE_1fSo)
* [BazelCon 2024: Not Going the Distance: Filtering Tests by Build Graph Distance](https://youtu.be/Or0o0Q7Zc1w?si=nIIkTH6TP-pcPoRx)
* [BazelCon 2025:
Precision CI at Scale: Target-Aware Workflows with Bazel Diff - Maxwell Elliott & Connor Wybranowski](https://youtu.be/rCFc3tFcVVE?si=WF8HdCyOBQEAHGL4)

## Star History

<a href="https://star-history.com/#Tinder/bazel-diff&Date">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=Tinder/bazel-diff&type=Date&theme=dark" />
    <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=Tinder/bazel-diff&type=Date" />
    <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=Tinder/bazel-diff&type=Date" />
  </picture>
</a>

## Running the tests

To run the tests simply run

```terminal
bazel test //...
```

## Code coverage

CI enforces a minimum **90% line coverage** on production sources. Kotlin
(`cli/src/main/...`) and Go (`tools/go/...`) are gated **independently** at 90% each, so
thin coverage in one language can't hide behind well-covered code in the other. To run the
same checks locally:

```terminal
make coverage
```

This invokes
`bazel coverage --combined_report=lcov //cli/... //tools:coverage_check_test //tools/go/...`
and then runs `//tools:coverage-check` twice against the resulting LCOV report — once for
the Kotlin main sources and once scoped to `tools/go/` (`--include tools/go/`). The check is
a Python `py_binary` ([`tools/coverage_check.py`](tools/coverage_check.py)) that prints a
per-file table sorted by coverage (worst first), the overall percentage, and exits
non-zero if the scoped coverage is below the threshold.

If you've already produced a coverage report and just want to re-check the threshold,
`make coverage-check` runs only the binary against `bazel-out/_coverage/_coverage_report.dat`.

The enforcement logic itself is tested under `//tools:coverage_check_test` — run it
directly with `make coverage-test` (or `bazel test //tools:coverage_check_test`).

For an interactive HTML report (annotated source with covered/uncovered lines
highlighted), use `make coverage-html`. This requires the `lcov` package
(`brew install lcov` on macOS, `apt-get install lcov` on Debian/Ubuntu) and writes
the report to `coverage-html/index.html`. The threshold gate still runs and still
sets the exit code — HTML is an additional artifact, not a replacement.

To experiment with a different threshold (e.g. while ratcheting up), set
`COVERAGE_THRESHOLD`:

```terminal
COVERAGE_THRESHOLD=80 make coverage
```

The CI matrix runs the same check on every Linux/macOS test job, so a PR cannot
land if it drops main-source line coverage below the threshold.

## Versioning

We use [SemVer](http://semver.org/) for versioning. For the versions available,
see the [tags on this repository](https://github.com/Tinder/bazel-diff/tags).

## License

---

```text
Copyright (c) 2020, Match Group, LLC
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:
    * Redistributions of source code must retain the above copyright
      notice, this list of conditions and the following disclaimer.
    * Redistributions in binary form must reproduce the above copyright
      notice, this list of conditions and the following disclaimer in the
      documentation and/or other materials provided with the distribution.
    * Neither the name of Match Group, LLC nor the names of its contributors
      may be used to endorse or promote products derived from this software
      without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL MATCH GROUP, LLC BE LIABLE FOR ANY
DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```
