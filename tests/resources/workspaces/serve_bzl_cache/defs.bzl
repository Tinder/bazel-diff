def make_generated(name):
    native.genrule(
        name = name,
        outs = [name + ".txt"],
        cmd = "echo generated > $@",
    )
