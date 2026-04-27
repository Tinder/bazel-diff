# bazel-diff

[![Build status](https://github.com/Tinder/bazel-diff/actions/workflows/ci.yaml/badge.svg?branch=master)](https://github.com/Tinder/bazel-diff/actions/workflows/ci.yaml)

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

<!-- BEGIN_SECTION: cli-help -->
## CLI Interface

`bazel-diff` Command

```terminal
Usage: bazel-diff [-hvV] [COMMAND]
Writes to a file the impacted targets between two Bazel graph JSON files
  -h, --help      Show this help message and exit.
  -v, --verbose   Display query string, missing files and elapsed time
  -V, --version   Print version information and exit.
Commands:
  generate-hashes       Writes to a file the SHA256 hashes for each Bazel
                          Target in the provided workspace.
  get-impacted-targets  Command-line utility to analyze the state of the bazel
                          build graph
```

### `generate-hashes` command

```terminal
Usage: bazel-diff generate-hashes [-hkvV] [--[no-]excludeExternalTargets] [--
                                  [no-]includeTargetType] [--[no-]useCquery]
                                  [-b=<bazelPath>]
                                  [--contentHashPath=<contentHashPath>]
                                  [--cqueryExpression=<cqueryExpression>]
                                  [-d=<depsMappingJSONPath>]
                                  [--fineGrainedHashExternalReposFile=<fineGrain
                                  edHashExternalReposFile>]
                                  [-m=<modifiedFilepaths>] [-s=<seedFilepaths>]
                                  -w=<workspacePath>
                                  [-co=<bazelCommandOptions>]...
                                  [--cqueryCommandOptions=<cqueryCommandOptions>
                                  ]...
                                  [--fineGrainedHashExternalRepos=<fineGrainedHa
                                  shExternalRepos>]...
                                  [--ignoredRuleHashingAttributes=<ignoredRuleHa
                                  shingAttributes>]...
                                  [-so=<bazelStartupOptions>]...
                                  [-tt=<targetType>[,<targetType>...]]...
                                  <outputPath>
Writes to a file the SHA256 hashes for each Bazel Target in the provided
workspace.
      <outputPath>        The filepath to write the resulting JSON of
                            dictionary target => SHA-256 values. If not
                            specified, the JSON will be written to STDOUT.
  -b, --bazelPath=<bazelPath>
                          Path to Bazel binary. If not specified, the Bazel
                            binary available in PATH will be used.
      -co, --bazelCommandOptions=<bazelCommandOptions>
                          Additional space separated Bazel command options used
                            when invoking `bazel query`
      --contentHashPath=<contentHashPath>
                          Path to content hash json file. It's a map which maps
                            relative file path from workspace path to its
                            content hash. Files in this map will skip content
                            hashing and use provided value
      --cqueryCommandOptions=<cqueryCommandOptions>
                          Additional space separated Bazel command options used
                            when invoking `bazel cquery`. This flag is has no
                            effect if `--useCquery`is false.
      --cqueryExpression=<cqueryExpression>
                          Custom cquery expression to use instead of the
                            default 'deps(//...:all-targets)'. This allows you
                            to exclude problematic targets (e.g., analysis_test
                            targets that are designed to fail). Example: 'deps
                            (//...:all-targets) except //path/to/failing:
                            target'. This flag has no effect if `--useCquery`
                            is false.
  -d, --depEdgesFile=<depsMappingJSONPath>
                          Path to the file where dependency edges are written
                            to. If not specified, the dependency edges will not
                            be written to a file. Needed for computing build
                            graph distance metrics. See bazel-diff docs for
                            more details about build graph distance metrics.
      --[no-]excludeExternalTargets
                          If true, exclude external targets (do not query
                            //external:all-targets). When Bzlmod is enabled
                            (detected via bazel mod graph), external targets
                            are excluded automatically. Set this when using
                            Bazel with --enable_workspace=false in other
                            configurations. Defaults to false.
      --fineGrainedHashExternalRepos=<fineGrainedHashExternalRepos>
                          Comma separate list of external repos in which
                            fine-grained hashes are computed for the targets.
                            By default, external repos are treated as an opaque
                            blob. If an external repo is specified here,
                            bazel-diff instead computes the hash for individual
                            targets. For example, one wants to specify `maven`
                            here if they user rules_jvm_external so that
                            individual third party dependency change won't
                            invalidate all targets in the mono repo.
      --fineGrainedHashExternalReposFile=<fineGrainedHashExternalReposFile>
                          A text file containing a newline separated list of
                            external repos. Similar to
                            --fineGrainedHashExternalRepos but helps you avoid
                            exceeding max arg length. Mutually exclusive with
                            --fineGrainedHashExternalRepos.
  -h, --help              Show this help message and exit.
      --ignoredRuleHashingAttributes=<ignoredRuleHashingAttributes>
                          Attributes that should be ignored when hashing rule
                            targets.
      --[no-]includeTargetType
                          Whether include target type in the generated JSON or
                            not.
                          If false, the generate JSON schema is: {"<target>":
                            "<sha256>"}
                          If true, the generate JSON schema is: {"<target>":
                            "<type>#<sha256>" }
  -k, --[no-]keep_going   This flag controls if `bazel query` will be executed
                            with the `--keep_going` flag or not. Disabling this
                            flag allows you to catch configuration issues in
                            your Bazel graph, but may not work for some Bazel
                            setups. Defaults to `true`
  -m, --modified-filepaths=<modifiedFilepaths>
                          Experimental: A text file containing a newline
                            separated list of filepaths (relative to the
                            workspace) these filepaths should represent the
                            modified files between the specified revisions and
                            will be used to scope what files are hashed during
                            hash generation.
  -s, --seed-filepaths=<seedFilepaths>
                          A text file containing a newline separated list of
                            filepaths. Each file in this list will be read and
                            its content will be used as a SHA256 seed when
                            determining affected targets in the build graph.
                            Invalidating any of these files will effectively
                            mark all targets as affected.
      -so, --bazelStartupOptions=<bazelStartupOptions>
                          Additional space separated Bazel client startup
                            options used when invoking Bazel
      -tt, --targetType=<targetType>[,<targetType>...]
                          The types of targets to filter. Use comma (,) to
                            separate multiple values, e.g.
                            '--targetType=SourceFile,Rule,GeneratedFile'.
      --[no-]useCquery    If true, use cquery instead of query when generating
                            dependency graphs. Using cquery would yield more
                            accurate build graph at the cost of slower query
                            execution. When this is set, one usually also wants
                            to set `--cqueryCommandOptions` to specify a
                            targeting platform. Note that this flag only works
                            with Bazel 6.2.0 or above because lower versions
                            does not support `--query_file` flag.
  -v, --verbose           Display query string, missing files and elapsed time
  -V, --version           Print version information and exit.
  -w, --workspacePath=<workspacePath>
                          Path to Bazel workspace directory.
```

### `get-impacted-targets` command

```terminal
Missing required options: '--startingHashes=<startingHashesJSONPath>', '--finalHashes=<finalHashesJSONPath>', '--workspacePath=<workspacePath>'
Usage: bazel-diff get-impacted-targets [-v] [--[no-]noBazelrc] [-b=<bazelPath>]
                                       [-d=<depsMappingJSONPath>]
                                       -fh=<finalHashesJSONPath>
                                       [-o=<outputPath>]
                                       -sh=<startingHashesJSONPath>
                                       -w=<workspacePath>
                                       [-so=<bazelStartupOptions>]...
                                       [-tt=<targetType>[,<targetType>...]]...
Command-line utility to analyze the state of the bazel build graph
  -b, --bazelPath=<bazelPath>
                         Path to Bazel binary. If not specified, the Bazel
                           binary available in PATH will be used.
  -d, --depEdgesFile=<depsMappingJSONPath>
                         Path to the file where dependency edges are. If
                           specified, build graph distance metrics will be
                           computed from the given hash data.
      -fh, --finalHashes=<finalHashesJSONPath>
                         The path to the JSON file of target hashes for the
                           final revision. Run 'generate-hashes' to get this
                           value.
      --[no-]noBazelrc   Don't use .bazelrc
  -o, --output=<outputPath>
                         Filepath to write the impacted Bazel targets to. If
                           using depEdgesFile: formatted in json, otherwise:
                           newline separated. If not specified, the output will
                           be written to STDOUT.
      -sh, --startingHashes=<startingHashesJSONPath>
                         The path to the JSON file of target hashes for the
                           initial revision. Run 'generate-hashes' to get this
                           value.
      -so, --bazelStartupOptions=<bazelStartupOptions>
                         Additional space separated Bazel client startup
                           options used when invoking Bazel
      -tt, --targetType=<targetType>[,<targetType>...]
                         The types of targets to filter. Use comma (,) to
                           separate multiple values, e.g.
                           '--targetType=SourceFile,Rule,GeneratedFile'.
  -v, --verbose          Display query string, missing files and elapsed time
  -w, --workspacePath=<workspacePath>
                         Path to Bazel workspace directory. Required for module
                           change detection.
```
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
bazel_dep(name = "bazel-diff", version = "19.0.1")
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
<table>
  <tr>
    <td align="center"><a href="https://github.com/tinder-maxwellelliott"><img src="https://avatars.githubusercontent.com/u/56700854?s=64" width="64" alt="Maxwell Elliott"/><br/><sub><b>Maxwell Elliott</b></sub></a></td>
    <td align="center"><a href="https://github.com/honnix"><img src="https://avatars.githubusercontent.com/u/158892?s=64" width="64" alt="Honnix"/><br/><sub><b>Honnix</b></sub></a></td>
    <td align="center"><a href="https://github.com/fa93hws"><img src="https://avatars.githubusercontent.com/u/10626756?s=64" width="64" alt="eric wang"/><br/><sub><b>eric wang</b></sub></a></td>
    <td align="center"><a href="https://github.com/fa93hws"><img src="https://avatars.githubusercontent.com/u/10626756?s=64" width="64" alt="Eric Wang"/><br/><sub><b>Eric Wang</b></sub></a></td>
    <td align="center"><a href="https://github.com/tgeng"><img src="https://avatars.githubusercontent.com/u/29584386?s=64" width="64" alt="Tianyu Geng"/><br/><sub><b>Tianyu Geng</b></sub></a></td>
    <td align="center"><a href="https://github.com/BalestraPatrick"><img src="https://avatars.githubusercontent.com/u/3658887?s=64" width="64" alt="Patrick Balestra"/><br/><sub><b>Patrick Balestra</b></sub></a></td>
  </tr>
  <tr>
    <td align="center"><a href="https://github.com/purkhusid"><img src="https://avatars.githubusercontent.com/u/5622403?s=64" width="64" alt="Daniel P. Purkhus"/><br/><sub><b>Daniel P. Purkhus</b></sub></a></td>
    <td align="center"><a href="https://github.com/alexeagle"><img src="https://avatars.githubusercontent.com/u/47395?s=64" width="64" alt="Alex Eagle"/><br/><sub><b>Alex Eagle</b></sub></a></td>
    <td align="center"><a href="https://github.com/Malinskiy"><img src="https://avatars.githubusercontent.com/u/2089114?s=64" width="64" alt="Anton Malinskiy"/><br/><sub><b>Anton Malinskiy</b></sub></a></td>
    <td align="center"><a href="https://github.com/sharmila-oai"><img src="https://avatars.githubusercontent.com/u/257629015?s=64" width="64" alt="Sharmila"/><br/><sub><b>Sharmila</b></sub></a></td>
    <td align="center"><a href="https://github.com/dkostyrev"><img src="https://avatars.githubusercontent.com/u/183590?s=64" width="64" alt="Dmitrii Kostyrev"/><br/><sub><b>Dmitrii Kostyrev</b></sub></a></td>
    <td align="center"><a href="https://github.com/jmthvt"><img src="https://avatars.githubusercontent.com/u/1737199?s=64" width="64" alt="Jérémy Mathevet"/><br/><sub><b>Jérémy Mathevet</b></sub></a></td>
  </tr>
  <tr>
    <td align="center"><a href="https://github.com/nikhilbirmiwal"><img src="https://avatars.githubusercontent.com/u/65141192?s=64" width="64" alt="Nikhil Birmiwal"/><br/><sub><b>Nikhil Birmiwal</b></sub></a></td>
    <td align="center"><a href="https://github.com/morozov"><img src="https://avatars.githubusercontent.com/u/59683?s=64" width="64" alt="Sergei Morozov"/><br/><sub><b>Sergei Morozov</b></sub></a></td>
    <td align="center"><a href="https://github.com/fahhem"><img src="https://avatars.githubusercontent.com/u/306100?s=64" width="64" alt="Fahrzin Hemmati"/><br/><sub><b>Fahrzin Hemmati</b></sub></a></td>
    <td align="center"><a href="https://github.com/JaimeLennox"><img src="https://avatars.githubusercontent.com/u/1424638?s=64" width="64" alt="Jaime Lennox"/><br/><sub><b>Jaime Lennox</b></sub></a></td>
    <td align="center"><a href="https://github.com/jmwachtel"><img src="https://avatars.githubusercontent.com/u/1046228?s=64" width="64" alt="jmwachtel"/><br/><sub><b>jmwachtel</b></sub></a></td>
    <td align="center"><a href="https://github.com/Ahajha"><img src="https://avatars.githubusercontent.com/u/44127594?s=64" width="64" alt="Alex Trotta"/><br/><sub><b>Alex Trotta</b></sub></a></td>
  </tr>
  <tr>
    <td align="center"><a href="https://github.com/nollbit"><img src="https://avatars.githubusercontent.com/u/99957?s=64" width="64" alt="Johan Mjönes"/><br/><sub><b>Johan Mjönes</b></sub></a></td>
    <td align="center"><a href="https://github.com/lucasteixeira-cb"><img src="https://avatars.githubusercontent.com/u/116316841?s=64" width="64" alt="Lucas Teixeira"/><br/><sub><b>Lucas Teixeira</b></sub></a></td>
    <td align="center"><a href="https://github.com/GuillaumeVW"><img src="https://avatars.githubusercontent.com/u/53425033?s=64" width="64" alt="Guillaume Van Wassenhove"/><br/><sub><b>Guillaume Van Wassenhove</b></sub></a></td>
    <td align="center"><a href="https://github.com/fmeum"><img src="https://avatars.githubusercontent.com/u/4312191?s=64" width="64" alt="Fabian Meumertzheim"/><br/><sub><b>Fabian Meumertzheim</b></sub></a></td>
    <td align="center"><a href="https://github.com/blockjon-dd"><img src="https://avatars.githubusercontent.com/u/117850895?s=64" width="64" alt="Jonathan Block"/><br/><sub><b>Jonathan Block</b></sub></a></td>
    <td align="center"><a href="https://github.com/alex-torok"><img src="https://avatars.githubusercontent.com/u/8749956?s=64" width="64" alt="Alex Torok"/><br/><sub><b>Alex Torok</b></sub></a></td>
  </tr>
  <tr>
    <td align="center"><a href="https://github.com/naveenOnarayanan"><img src="https://avatars.githubusercontent.com/u/3528131?s=64" width="64" alt="Naveen Narayanan"/><br/><sub><b>Naveen Narayanan</b></sub></a></td>
    <td align="center"><a href="https://github.com/OniOni"><img src="https://avatars.githubusercontent.com/u/385657?s=64" width="64" alt="Mathieu Sabourin"/><br/><sub><b>Mathieu Sabourin</b></sub></a></td>
    <td align="center"><a href="https://github.com/andre-alves"><img src="https://avatars.githubusercontent.com/u/7773955?s=64" width="64" alt="André"/><br/><sub><b>André</b></sub></a></td>
    <td align="center"><a href="https://github.com/bz-canva"><img src="https://avatars.githubusercontent.com/u/125319243?s=64" width="64" alt="Boris"/><br/><sub><b>Boris</b></sub></a></td>
    <td align="center"><a href="https://github.com/chenrui333"><img src="https://avatars.githubusercontent.com/u/1580956?s=64" width="64" alt="Rui Chen"/><br/><sub><b>Rui Chen</b></sub></a></td>
    <td align="center"><a href="https://github.com/sanju-naik"><img src="https://avatars.githubusercontent.com/u/66404008?s=64" width="64" alt="Sanju Naik"/><br/><sub><b>Sanju Naik</b></sub></a></td>
  </tr>
  <tr>
    <td align="center"><a href="https://github.com/thirtyseven"><img src="https://avatars.githubusercontent.com/u/123678?s=64" width="64" alt="Ted Kaplan"/><br/><sub><b>Ted Kaplan</b></sub></a></td>
    <td align="center"><a href="https://github.com/lalten"><img src="https://avatars.githubusercontent.com/u/11611719?s=64" width="64" alt="Laurenz"/><br/><sub><b>Laurenz</b></sub></a></td>
    <td align="center"><a href="https://github.com/molar"><img src="https://avatars.githubusercontent.com/u/1433210?s=64" width="64" alt="mla"/><br/><sub><b>mla</b></sub></a></td>
    <td align="center"><a href="https://github.com/tinder-yukisawa"><img src="https://avatars.githubusercontent.com/u/54122444?s=64" width="64" alt="tinder-yukisawa"/><br/><sub><b>tinder-yukisawa</b></sub></a></td>
    <td align="center"><a href="https://github.com/KevinJiao"><img src="https://avatars.githubusercontent.com/u/9851473?s=64" width="64" alt="Kevin Jiao"/><br/><sub><b>Kevin Jiao</b></sub></a></td>
    <td align="center"><a href="https://github.com/vcase"><img src="https://avatars.githubusercontent.com/u/10698795?s=64" width="64" alt="Vincent Case"/><br/><sub><b>Vincent Case</b></sub></a></td>
  </tr>
  <tr>
    <td align="center"><a href="https://github.com/fh-wpanfil"><img src="https://avatars.githubusercontent.com/u/262680997?s=64" width="64" alt="Walt Panfil"/><br/><sub><b>Walt Panfil</b></sub></a></td>
    <td align="center"><a href="https://github.com/mehran-prs"><img src="https://avatars.githubusercontent.com/u/22454054?s=64" width="64" alt="Mehran Poursadeghi"/><br/><sub><b>Mehran Poursadeghi</b></sub></a></td>
  </tr>
</table>
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
