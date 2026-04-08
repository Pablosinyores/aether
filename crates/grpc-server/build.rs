fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut prost_config = prost_build::Config::new();
    prost_config.bytes(["."]);

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos_with_config(prost_config, &["../../proto/aether.proto"], &["../../proto"])?;
    Ok(())
}
