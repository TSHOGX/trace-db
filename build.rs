// The proto build step, its vendored `protoc`, and the generated client and
// server all belong to the `grpc` feature. A build script sees features as
// `CARGO_FEATURE_*` in its environment, and Cargo also applies `cfg(feature)`
// to the script itself, so gating the body here is what keeps an embedder
// building with `--no-default-features` from needing `protoc` at all.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "grpc")]
    {
        let protoc = protoc_bin_vendored::protoc_bin_path()?;
        std::env::set_var("PROTOC", protoc);
        tonic_build::configure()
            .build_client(true)
            .build_server(true)
            .compile_protos(&["proto/tracedb/v1/tracedb.proto"], &["proto"])?;
        println!("cargo:rerun-if-changed=proto/tracedb/v1/tracedb.proto");
    }
    Ok(())
}
