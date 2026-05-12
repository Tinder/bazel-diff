# Reproducer workspace for https://github.com/Tinder/bazel-diff/issues/197.
#
# Shape:
#   inner_repo  --> filegroup wrapping data.txt
#   middle_repo --> alias re-exporting @inner_repo//:all_files
#   //:consumer --> depends on @middle_repo//:wrapped (not directly on inner_repo)
#
# When inner_repo/data.txt changes, //:consumer should be reported as impacted,
# even if --fineGrainedHashExternalRepos only mentions @inner_repo (i.e. users
# do not have to enumerate every wrapping repo). @Ahajha's comment in #197 calls
# this out as the still-broken case after PR #322.
genrule(
    name = "consumer",
    srcs = ["@middle_repo//:wrapped"],
    outs = ["consumer.out"],
    cmd = "cat $(SRCS) > $@",
)
