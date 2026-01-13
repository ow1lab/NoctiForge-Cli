use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use custom::CustomBuild;
use crate::api::{
    controlplane::{
        SetDigestToNameRequest, control_plane_service_client::ControlPlaneServiceClient,
    },
    registry::{self, RegistryPushRequest},
};
use registry::registry_service_client::RegistryServiceClient;
use rust::RustBuild;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, duplex};
use tonic::{Request, async_trait};
use tracing::{debug, error, info};

use crate::command::push::rust::RustBuildConfig;

mod custom;
mod rust;

const CONFIG_FILE: &str = "Nocti.toml";

#[async_trait]
trait BuildService {
    async fn build(&self, project_path: PathBuf, temp_path: PathBuf) -> anyhow::Result<()>;
}

#[derive(Debug, Deserialize)]
struct Project {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Config {
    project: Project,
    build: Build,
    #[serde(default = "default_registry_url")]
    registry_url: String,
    #[serde(default = "default_control_plane_url")]
    control_plane_url: String,
}

fn default_registry_url() -> String {
    std::env::var("NOCTI_REGISTRY_URL").unwrap_or_else(|_| "http://localhost:50001".to_string())
}

fn default_control_plane_url() -> String {
    std::env::var("NOCTI_CONTROL_PLANE_URL")
        .unwrap_or_else(|_| "http://localhost:50002".to_string())
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum Build {
    #[serde(rename = "custom")]
    Custom(CustomBuild),
    #[serde(rename = "rust")]
    Rust(RustBuildConfig),
}

pub async fn run(path: &str) -> Result<()> {
    let project_path = Path::new(path);
    info!("Running push command on path: {:?}", project_path);

    // Validate project path
    if !project_path.is_dir() {
        error!("Provided path is invalid: {:?}", project_path);
        bail!("path does not exist or is not a directory");
    }

    // Validate config file exists
    let config_file_path = project_path.join(CONFIG_FILE);
    if !config_file_path.is_file() {
        error!("Missing config file at: {:?}", config_file_path);
        bail!("'{}' does not exist or is not a file", CONFIG_FILE);
    }

    // Load and parse config
    info!("Loading project config from: {:?}", config_file_path);
    let config_content = std::fs::read_to_string(&config_file_path)
        .with_context(|| format!("Failed to read config file: {:?}", config_file_path))?;

    let config: Config =
        toml::from_str(&config_content).context("Failed to parse config file as TOML")?;

    debug!("Parsed config: {:?}", config);

    // Create build service
    let buildservice: Box<dyn BuildService + Send + Sync> = match config.build {
        Build::Custom(cb) => {
            debug!("Using custom build");
            Box::new(cb)
        }
        Build::Rust(rb_config) => {
            debug!("Using Rust build with config: {:?}", rb_config);
            Box::new(RustBuild::from(rb_config))
        }
    };

    // Create temporary directory for build output
    debug!("Creating temporary directory for build artifacts");
    let temp_dir = tempfile::Builder::new()
        .prefix("nocti-build-")
        .tempdir()
        .context("Failed to create temporary directory")?;

    let temp_path = temp_dir.path().to_path_buf();
    debug!("Temporary directory created at: {:?}", temp_path);

    // Run the build
    info!("Starting build...");
    buildservice
        .build(project_path.to_path_buf(), temp_path.clone())
        .await
        .context("Build failed")?;
    info!("Build completed successfully");

    // Create tar archive and stream it
    let (writer, mut reader) = duplex(8 * 1024);
    info!("Creating in-memory tar archive...");

    let tar_task = tokio::spawn(async move {
        let temp_path = temp_dir.path();

        let mut builder = tokio_tar::Builder::new(writer);
        if let Err(e) = builder.append_dir_all(".", temp_path).await {
            error!("Failed to add directory to tar: {}", e);
            return Err(anyhow::anyhow!("tar append_dir_all error: {}", e));
        }
        if let Err(e) = builder.finish().await {
            error!("Failed to finalize tar archive: {}", e);
            return Err(anyhow::anyhow!("tar finish error: {}", e));
        }
        debug!("Tarball creation completed successfully");
        Ok(())
    });

    // Create a stream of RegistryPushRequest from reader
    let outbound = async_stream::stream! {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => {
                    debug!("Finished reading all tar data");
                    break;
                }
                Ok(n) => {
                    debug!("Read {} bytes from tar stream", n);
                    let req = RegistryPushRequest {
                        data: buf[..n].to_vec(),
                    };
                    yield req;
                }
                Err(e) => {
                    error!("Error reading from tar stream: {}", e);
                    break;
                }
            }
        }
    };

    // Connect to registry and push
    info!(
        "Connecting to RegistryService at {}...",
        config.registry_url
    );
    let mut registry_client = RegistryServiceClient::connect(config.registry_url.clone())
        .await
        .with_context(|| {
            format!(
                "Failed to connect to RegistryService at {}",
                config.registry_url
            )
        })?;

    info!("Sending tar data to registry...");
    let response = registry_client
        .push(Request::new(outbound))
        .await
        .context("Failed to push to registry")?
        .into_inner();

    debug!("Registry responded with digest: {}", response.digest);

    // Wait for tar task to complete
    tar_task.await.context("Tar creation task panicked")??;

    // Associate digest with project name
    let key = config.project.name;
    info!("Associating digest with project key: {}", key);

    let mut control_plane_client =
        ControlPlaneServiceClient::connect(config.control_plane_url.clone())
            .await
            .with_context(|| {
                format!(
                    "Failed to connect to ControlPlaneService at {}",
                    config.control_plane_url
                )
            })?;

    let request = SetDigestToNameRequest {
        key: key.clone(),
        digest: response.digest,
    };

    let response = control_plane_client
        .set_digest_to_name(Request::new(request))
        .await
        .context("Failed to set digest to name mapping")?
        .into_inner();

    if response.success {
        info!("Successfully set digest for key '{}'", key);
        Ok(())
    } else {
        error!("Failed to associate digest with key '{}'", key);
        bail!("Control plane rejected digest to name mapping")
    }
}
