def forward(name):
    left = "@@//:left.txt"
    right = "@@//:right.txt"
    native.alias(
        name = name,
        actual = select({"@@//:alternate": right, "//conditions:default": left}),
        visibility = ["//visibility:public"],
    )
