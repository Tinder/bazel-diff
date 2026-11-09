def _source(r):
    r.file("BUILD.bazel", 'exports_files(["raw.h"])')
    r.file("raw.h", r.attr.text)
    return r.repo_metadata(reproducible = True)

_source_repo = repository_rule(implementation = _source, attrs = {"text": attr.string(mandatory = True)})

def _sdk(r):
    r.file("sdk.h", r.read(Label("@@+sdk_inputs+upstream_data//:raw.h")))
    r.file("BUILD.bazel", 'exports_files(["sdk.h"])\nfilegroup(name = "headers", srcs = ["sdk.h"], visibility = ["//visibility:public"])')
    return r.repo_metadata(reproducible = True)

_sdk_repo = repository_rule(implementation = _sdk)

def _hub(r):
    r.file("BUILD.bazel", 'alias(name = "raw", actual = "@upstream_data//:raw.h", visibility = ["//visibility:public"])')
    return r.repo_metadata(reproducible = True)

_hub_repo = repository_rule(implementation = _hub)

def _extension(m):
    versions = json.decode(m.read(Label("//:versions.json")))
    _source_repo(name = "upstream_data", text = versions["data"])
    _source_repo(name = "unrelated_data", text = versions["unrelated"])
    _sdk_repo(name = "vendored")
    _hub_repo(name = "public")

sdk_inputs = module_extension(implementation = _extension)
