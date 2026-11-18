"""Stand-in for a documentation toolchain such as Stardoc."""

def doc(name):
    native.filegroup(name = name)
