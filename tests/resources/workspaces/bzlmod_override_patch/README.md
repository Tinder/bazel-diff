# Override patch regression (#522)

The tests in `tests/e2e/external.rs` create a local registry and archive for
`patched_dep`, then change only `dep.patch`. The patch changes `data.txt`, so
Starlark rule hashes cannot reveal the change.

Both query and cquery must report `//:consumer` as impacted, leave `//:unrelated`
unchanged, and produce stable hashes on repeated runs. The tests also build the
consumer before and after editing the patch to verify that its output changes.
No fine-grained external repository configuration is used.

Run with a Bazel version supporting `mod show_repo`:

```sh
bazel test //tests:e2e_test_external_bzlmod_override_patch_impacts_consumer \
  //tests:e2e_test_external_bzlmod_override_patch_impacts_consumer_cquery
```

Repository input content hashing (#520) covers the patch itself. Cquery uses
canonical labels, so its synthetic repository dependencies must also resolve
under canonical names; otherwise the changed repository hash never reaches the
consumer. This fixture reproduces that missing dependency edge.
