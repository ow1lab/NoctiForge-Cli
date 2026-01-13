use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

fn main() -> Result<(), Box<dyn Error>> {
    let out_dir = std::env::var("OUT_DIR")?;
    let api_dir = Path::new(&out_dir).join("api");

    tonic_prost_build::configure().compile_protos(
        &[
            get_proto_file("controlplane", "controlplane.proto", &api_dir)?,
            get_proto_file("registry", "registry.proto", &api_dir)?,
            get_proto_file("worker", "worker.proto", &api_dir)?,
        ],
        &[api_dir],
    )?;

    Ok(())
}

fn get_proto_file(service: &str, proto: &str, api_dir: &Path) -> Result<PathBuf, Box<dyn Error>> {
    fs::create_dir_all(api_dir)?;

    let proto_path = api_dir.join(proto);
    let proto_url = format!(
        "https://raw.githubusercontent.com/ow1lab/NoctiForge-Proto/refs/heads/main/{}/v0/{}",
        service, proto
    );

    let response = reqwest::blocking::get(proto_url)?.text()?;
    fs::write(&proto_path, response)?;

    Ok(proto_path)
}
