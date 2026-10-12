# Reproducer workspace for https://github.com/Tinder/bazel-diff/issues/510.
#
# Shape:
#   //:consumer --> @facade//:export
#               --> @@middle+//:wrapped   (apparent name only inside facade)
#               --> @leaf//:value
#               --> @leaf//:value.txt
#
# Unlike wrapped_external_repo (#197), the root never declares `middle`: facade
# pulls it in under the private apparent name `private_wrapper`, so `@middle`
# and `@private_wrapper` are not valid from the root. When leaf/value.txt
# changes with --fineGrainedHashExternalRepos=@leaf, //:consumer should be
# impacted without the caller enumerating @facade and @@middle+ too.
filegroup(
    name = "consumer",
    srcs = ["@facade//:export"],
)

filegroup(
    name = "control",
    srcs = ["control.txt"],
)
