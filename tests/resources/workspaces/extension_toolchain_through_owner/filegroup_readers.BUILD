load("@app//:files.bzl", "COPIES")

genrule(
    name = "copy_reader",
    srcs = [COPIES],
    outs = ["plain.txt"],
    cmd = "cp $< $@",
)

genrule(
    name = "tool_reader",
    srcs = [COPIES],
    outs = ["mixed.txt"],
    cmd = "cat $< $(location @app//:lib) > $@",
    tools = ["@app//:lib"],
)

filegroup(
    name = "unrelated",
    srcs = ["unrelated.txt"],
)
