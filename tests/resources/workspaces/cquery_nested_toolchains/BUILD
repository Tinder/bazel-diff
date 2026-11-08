load(":defs.bzl", "inner", "outer", "reader")

constraint_setting(name = "flavor")
constraint_value(name = "x", constraint_setting = ":flavor")
constraint_value(name = "y", constraint_setting = ":flavor")
platform(name = "platform_x", constraint_values = [":x"])
platform(name = "platform_y", constraint_values = [":y"])

toolchain_type(name = "inner_type")
toolchain_type(name = "outer_type")

inner(name = "inner_x", payload = "x.txt")
inner(name = "inner_y", payload = "y.txt")
outer(name = "outer_impl")

toolchain(name = "inner_reg_x", toolchain_type = ":inner_type", toolchain = ":inner_x", exec_compatible_with = [":x"])
toolchain(name = "inner_reg_y", toolchain_type = ":inner_type", toolchain = ":inner_y", exec_compatible_with = [":y"])
toolchain(name = "outer_reg", toolchain_type = ":outer_type", toolchain = ":outer_impl")

reader(name = "only_x", exec_compatible_with = [":x"])
reader(name = "only_y", exec_compatible_with = [":y"])

genrule(
    name = "joined",
    srcs = [":only_x", ":only_y"],
    outs = ["joined.txt"],
    cmd = "cat $(location :only_x) $(location :only_y) > $@",
)
