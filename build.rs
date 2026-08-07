fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path()?;
        std::env::set_var("PROTOC", protoc);
    }
    let build_proto = std::env::var("BAZEL_DIFF_BUILD_PROTO")
        .unwrap_or_else(|_| "rust/proto/build.proto".to_owned());
    let analysis_proto = std::env::var("BAZEL_DIFF_ANALYSIS_PROTO")
        .unwrap_or_else(|_| "rust/proto/analysis_v2.proto".to_owned());
    let include = std::path::Path::new(&build_proto)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("rust/proto"));
    buffa_build::Config::new()
        .files(&[&build_proto, &analysis_proto])
        .includes(&[include])
        .include_file("proto.rs")
        .generate_views(false)
        .compile()?;
    println!("cargo:rerun-if-changed=rust/proto/build.proto");
    println!("cargo:rerun-if-changed=rust/proto/analysis_v2.proto");
    Ok(())
}
