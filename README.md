# bazel-diff

test CLA

`bazel-diff` is a command line tool for Bazel projects that allows users to determine the exact affected set of impacted targets between two Git revisions. Using this set, users can test or build the exact modified set of targets.

`bazel-diff` offers several key advantages over rolling your own target diffing solution

1. `bazel-diff` is designed for very large Bazel projects. We use Java Protobuf's `parseDelimitedFrom` method alongside Bazel Query's `streamed_proto` output option. These two together allow you to parse Gigabyte or larger protobuf messages. We have tested it with projects containing tens of thousands of targets.
2. We avoid usage of large command line query lists when interacting with Bazel, [issue here](https://github.com/bazelbuild/bazel/issues/8609). When you interact with Bazel with thousands of query parameters you can reach an upper maximum limit, seeing this error:

        bash: /usr/local/bin/bazel: Argument list too long

    `bazel-diff` is smart enough to prevent these errors
3. `bazel-diff` has been tested with file renames, deletions, and modifications. Works on `bzl` files, `WORKSPACE` files, `BUILD` files and regular files

Track the feature request for target diffing in Bazel [here](https://github.com/bazelbuild/bazel/issues/7962)

This approach was inspired by the [following BazelConf talk](https://www.youtube.com/watch?v=9Dk7mtIm7_A) by Benjamin Peterson.

### Prerequisites

* Git
* Bazel 3.3.0 or higher
* `bazel_skylib` version 1.0.2 or higher
* Java 8 JDK or higher (Bazel requires this)

## Getting Started

To start using `bazel-diff` immediately, simply clone down the repo and then run the example shell script:

    git clone https://github.com/Tinder/bazel-diff.git
    cd bazel-diff
    ./bazel-diff-example.sh WORKSPACE_PATH BAZEL_PATH START_GIT_REVISION END_GIT_REVISION

Here is a breakdown of those arguments:
* `WORKSPACE_PATH`: Path to directory containing your `WORKSPACE` file in your Bazel project.
  *  Note: Your project must use Git for `bazel-diff` to work!
* `BAZEL_PATH`: Path to your Bazel executable
* `START_GIT_REVISION`: Starting Git Branch or SHA for your desired commit range
* `END_GIT_REVISION`: Final Git Branch or SHA for your desired commit range

You can see the example shell script in action below:

![Demo](demo.gif)

Open `bazel-diff-example.sh` to see how this is implemented. This is purely an example use-case, but it is a great starting point to using `bazel-diff`.

## How it works

`bazel-diff` works as follows
* The `modified-filepaths` command is used to fetch the modified filepaths between two Git revisions, we write this list of modified filepaths to a file.

* The previous revision is checked out, then we run `generate-hashes`. This gives us the hashmap representation for the entire Bazel graph, then we write this JSON to a file. We do not pass `modified-filepaths` here since it is the starting revision.

* Next we checkout the initial revision, then we run `generate-hashes` with the file output of `modified-filepaths` and write that JSON to a file. Now we have our final hashmap representation for the Bazel graph.

* We run `bazel-diff` on the starting and final JSON hash filepaths to get our affected set of targets. This impacted set of targets is written to a file

* Finally we run `impacted-tests` with the filepath to the list of impacted targets. This returns us the impacted test targets.

## CLI Interface

`bazel-diff` Command
~~~
Usage: bazel-diff [-hV] -b=<bazelPath> [-fh=<finalHashesJSONPath>]
                  [-o=<outputPath>] [-sh=<startingHashesJSONPath>]
                  -w=<workspacePath> [COMMAND]
Writes to a file the impacted targets between two Bazel graph JSON files
  -b, --bazelPath=<bazelPath>
                  Path to Bazel binary
      -fh, --finalHashes=<finalHashesJSONPath>
                  The path to the JSON file of target hashes for the final
                    revision. Run 'generate-hashes' to get this value.
  -h, --help      Show this help message and exit.
  -o, --output=<outputPath>
                  Filepath to write the impacted Bazel targets to, newline
                    separated
      -sh, --startingHashes=<startingHashesJSONPath>
                  The path to the JSON file of target hashes for the initial
                    revision. Run 'generate-hashes' to get this value.
  -V, --version   Print version information and exit.
  -w, --workspacePath=<workspacePath>
                  Path to Bazel workspace directory.
~~~

`impacted-tests` Command
~~~
Usage: bazel-diff impacted-tests [-hV] -b=<bazelPath> -w=<workspacePath>
                                 <impactedBazelTargetsPath> <outputPath>
Write to a file the impacted test targets for the list of Bazel targets in the
provided file
      <impactedBazelTargetsPath>
                     The filepath to a newline separated list of Bazel targets
      <outputPath>   The filepath to write the impacted test targets to
  -b, --bazelPath=<bazelPath>
                     Path to Bazel binary
  -h, --help         Show this help message and exit.
  -V, --version      Print version information and exit.
  -w, --workspacePath=<workspacePath>
                     Path to Bazel workspace directory.
~~~

`modified-filepaths` Command
~~~
Usage: bazel-diff modified-filepaths [-hV] -b=<bazelPath> -w=<workspacePath>
                                     <startingGitRevision> <endingGitRevision>
                                     <outputPath>
Writes to the file the modified filepaths between two revisions.
      <startingGitRevision> The starting Git revision, e.g. "HEAD^"
      <endingGitRevision>   The final Git revision, e.g. "HEAD"
      <outputPath>          Path that the list of modified files will be
                              written to
  -b, --bazelPath=<bazelPath>
                            Path to Bazel binary
  -h, --help                Show this help message and exit.
  -V, --version             Print version information and exit.
  -w, --workspacePath=<workspacePath>
                            Path to Bazel workspace directory.
~~~

`generate-hashes` Command
~~~
Usage: bazel-diff generate-hashes [-hV] -b=<bazelPath> [-m=<modifiedFilepaths>]
                                  -w=<workspacePath> <outputPath>
Writes to a file the SHA256 hashes for each Bazel Target in the provided
workspace.
      <outputPath>   The filepath to write the resulting JSON of dictionary
                       target => SHA-256 values
  -b, --bazelPath=<bazelPath>
                     Path to Bazel binary
  -h, --help         Show this help message and exit.
  -m, --modifiedFilepaths=<modifiedFilepaths>
                     The path to a file containing the list of modified
                       filepaths in the workspace, you can use the
                       'modified-filepaths' command to get this list
  -V, --version      Print version information and exit.
  -w, --workspacePath=<workspacePath>
                     Path to Bazel workspace directory.
~~~

## Installing

### Run from Source

After cloning down the repo, you are good to go, Bazel will handle the rest

To run the project

```
bazel run :bazel-diff -- bazel-diff -h
```

### Run Via JAR Release

    curl -LO bazel-diff.jar GITHUB_RELEASE_JAR_URL
    java -jar bazel-diff.jar -h

### Build your own deployable JAR

```
bazel build //src/main/java/com/bazel-diff:bazel-diff_deploy.jar
java -jar bazel-bin/src/main/java/com/bazel-diff/bazel-diff_deploy.jar # This JAR can be run anywhere
```

### Integrate directly into your Bazel Project

Add the following to your `WORKSPACE` file to add the external repositories, replacing the `RELEASE_ARCHIVE_URL` with the archive url of the bazel-diff release you wish to depend on:

    load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive", "http_file")

    http_archive(
      name = "bazel_diff",
      urls = [
            "RELEASE_ARCHIVE_URL",
        ],
        sha256 = "UPDATE_ME",
    )

    load("@bazel_diff//:repositories.bzl", "bazel_diff_dependencies")
    load("@bazel_diff//:constants.bzl", "bazel_diff_dependencies")

    http_archive(
        name = "bazel_skylib",
        urls = [
            "ARCHIVE_URL_VERSION_ABOVE_1.0.2",
        ],
        sha256 = "UPDATE_ME",
    )

    load("@bazel_skylib//:workspace.bzl", "bazel_skylib_workspace")

    bazel_skylib_workspace()

    load("@bazel_skylib//lib:versions.bzl", "versions")

    load("@bazel_diff//:repositories.bzl", "bazel_diff_dependencies")

    # Allows bazel-diff to run based on your projects' version of Bazel
    bazel_diff_dependencies(versions.get())

    load("@rules_jvm_external//:defs.bzl", "maven_install")
    load("@bazel_diff//:constants.bzl", "BAZEL_DIFF_MAVEN_ARTIFACTS")

    maven_install(
        name = "bazel_diff_maven",
        artifacts = BAZEL_DIFF_MAVEN_ARTIFACTS,
        repositories = [
            "http://uk.maven.org/maven2",
            "https://jcenter.bintray.com/",
        ],
    )

Now you can simply run `bazel-diff` from your project:

```
bazel run @bazel_diff//:bazel-diff -- bazel-diff -h
```

## Running the tests

To run the tests simply run

```
bazel test //test/...
```

## Versioning

We use [SemVer](http://semver.org/) for versioning. For the versions available,
see the [tags on this repository](https://github.com/Tinder/bazel-diff/tags).

License
---
~~~
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
