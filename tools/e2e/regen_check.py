"""Fails if the generated e2e case lists no longer match the e2e sources.

Thin wrapper over `split_e2e_tests --check` so the gate is an ordinary py_test:
`bazel test //tools/e2e:regen_check`. See split_e2e_tests.py for what it
compares and tools/e2e/README.md for why.
"""

import sys

import split_e2e_tests

if __name__ == "__main__":
    try:
        sys.exit(split_e2e_tests.main(["--check"]))
    except split_e2e_tests.GeneratorError as error:
        print("error: {}".format(error), file=sys.stderr)
        sys.exit(2)
