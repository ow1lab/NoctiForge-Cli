use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;
use tokio::fs;
use tokio::process::Command;
use tonic::async_trait;
use tracing::debug;

use super::BuildService;

#[derive(Deserialize, Debug)]
pub struct RustBuildConfig {
    /// Target triple (e.g., "x86_64-unknown-linux-musl")
    #[serde(default)]
    target: Option<String>,

    /// Build profile: "release" or "debug"
    #[serde(default = "default_profile")]
    profile: String,

    /// Expected package name (for workspaces)
    #[serde(default)]
    package_name: Option<String>,

    /// Expected binary name
    #[serde(default)]
    binary_name: Option<String>,
}

fn default_profile() -> String {
    "release".to_string()
}

impl From<RustBuildConfig> for RustBuild {
    fn from(config: RustBuildConfig) -> Self {
        let profile = match config.profile.to_lowercase().as_str() {
            "debug" => BuildProfile::Debug,
            "release" => BuildProfile::Release,
            _ => {
                debug!(
                    "Unknown profile '{}', defaulting to Release",
                    config.profile
                );
                BuildProfile::Release
            }
        };

        let mut builder = RustBuild::new().profile(profile);

        if let Some(target) = config.target {
            builder = builder.target(target);
        }

        if let Some(package_name) = config.package_name {
            builder = builder.package_name(package_name);
        }

        if let Some(binary_name) = config.binary_name {
            builder = builder.binary_name(binary_name);
        }

        builder
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct CargoMetadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    manifest_path: String,
    #[serde(default)]
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Target {
    name: String,
    kind: Vec<String>,
}

/// Configuration for Rust builds
#[derive(Debug, Clone)]
pub struct RustBuild {
    /// Target triple (e.g., "x86_64-unknown-linux-musl")
    /// If None, uses the default target
    pub target: Option<String>,

    /// Build profile (release or debug)
    pub profile: BuildProfile,

    /// Expected package name (if None, uses workspace root or first package)
    pub package_name: Option<String>,

    /// Expected binary name (if None, finds first binary target)
    pub binary_name: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum BuildProfile {
    Release,
    Debug,
}

impl Default for RustBuild {
    fn default() -> Self {
        Self {
            target: Some("x86_64-unknown-linux-musl".to_string()),
            profile: BuildProfile::Release,
            package_name: None,
            binary_name: None,
        }
    }
}

impl RustBuild {
    /// Create a new RustBuild with default target
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the target
    pub fn target(mut self, target: String) -> Self {
        self.target = Some(target);
        self
    }

    /// Set the build profile
    pub fn profile(mut self, profile: BuildProfile) -> Self {
        self.profile = profile;
        self
    }

    /// Set the expected package name
    pub fn package_name(mut self, name: impl Into<String>) -> Self {
        self.package_name = Some(name.into());
        self
    }

    /// Set the expected binary name
    pub fn binary_name(mut self, name: impl Into<String>) -> Self {
        self.binary_name = Some(name.into());
        self
    }
}

#[async_trait]
impl BuildService for RustBuild {
    async fn build(&self, project_path: PathBuf, temp_path: PathBuf) -> anyhow::Result<()> {
        // Validate project structure
        self.validate_project(&project_path).await?;

        // Get package metadata
        let metadata = get_metadata(&project_path).await?;

        // Find the target package
        let package = self.find_package(&metadata, &project_path)?;

        // Find the binary target
        let binary_target = self.find_binary_target(package)?;

        // Run cargo build
        self.run_cargo_build(&project_path).await?;

        // Determine binary path
        let binary_path = self.get_binary_path(&project_path, &binary_target.name);

        // Validate binary exists
        self.validate_binary_exists(&binary_path).await?;

        // Copy binary to output
        self.copy_binary(&binary_path, &temp_path).await?;

        Ok(())
    }
}

impl RustBuild {
    /// Validate that the project has required files and cargo is available
    async fn validate_project(&self, project_path: &Path) -> anyhow::Result<()> {
        // Check if Cargo.toml exists
        let cargo_toml = project_path.join("Cargo.toml");
        if !cargo_toml.exists() {
            anyhow::bail!("No Cargo.toml found at {:?}", cargo_toml);
        }

        // Verify cargo is available
        let cargo_check = Command::new("cargo").arg("--version").output().await;

        if cargo_check.is_err() {
            anyhow::bail!(
                "cargo command not found. Please ensure Rust is installed and cargo is in PATH"
            );
        }

        Ok(())
    }

    /// Find the target package in the metadata
    fn find_package<'a>(
        &self,
        metadata: &'a CargoMetadata,
        project_path: &Path,
    ) -> anyhow::Result<&'a Package> {
        // If package name is specified, find by name
        if let Some(ref name) = self.package_name {
            return metadata
                .packages
                .iter()
                .find(|p| p.name == *name)
                .with_context(|| format!("Package '{}' not found in workspace", name));
        }

        // Try to find package at the project root
        let cargo_toml_path = project_path.join("Cargo.toml");
        if let Some(package) = metadata
            .packages
            .iter()
            .find(|p| Path::new(&p.manifest_path) == cargo_toml_path)
        {
            return Ok(package);
        }

        // Fall back to first package
        metadata
            .packages
            .first()
            .context("No packages found in cargo metadata. Is this a valid Rust project?")
    }

    /// Find the binary target in the package
    fn find_binary_target<'a>(&self, package: &'a Package) -> anyhow::Result<&'a Target> {
        // If binary name is specified, find by name
        if let Some(ref name) = self.binary_name {
            return package
                .targets
                .iter()
                .find(|t| t.kind.contains(&"bin".to_string()) && t.name == *name)
                .with_context(|| {
                    format!(
                        "Binary target '{}' not found in package '{}'",
                        name, package.name
                    )
                });
        }

        // Find first binary target
        package
            .targets
            .iter()
            .find(|t| t.kind.contains(&"bin".to_string()))
            .with_context(|| {
                format!(
                    "No binary targets found in package '{}'. Available targets: {:?}",
                    package.name,
                    package.targets.iter().map(|t| &t.name).collect::<Vec<_>>()
                )
            })
    }

    /// Run cargo build command
    async fn run_cargo_build(&self, project_path: &Path) -> anyhow::Result<()> {
        let mut cmd = Command::new("cargo");
        cmd.arg("build");

        // Add profile argument
        match self.profile {
            BuildProfile::Release => {
                cmd.arg("--release");
            }
            BuildProfile::Debug => {
                // Debug is default, no flag needed
            }
        }

        // Add target if specified
        if let Some(ref target) = self.target {
            cmd.arg("--target").arg(target);
        }

        cmd.current_dir(project_path)
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());

        let status = cmd.status().await.with_context(|| {
            format!(
                "Failed to execute cargo build in directory: {:?}",
                project_path
            )
        })?;

        if !status.success() {
            anyhow::bail!(
                "cargo build failed with exit code: {}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            );
        }

        Ok(())
    }

    /// Get the path where the binary should be located
    fn get_binary_path(&self, project_path: &Path, binary_name: &str) -> PathBuf {
        let mut path = project_path.join("target");

        // Add target triple directory if specified
        if let Some(ref target) = self.target {
            path = path.join(target);
        }

        // Add profile directory
        match self.profile {
            BuildProfile::Release => path = path.join("release"),
            BuildProfile::Debug => path = path.join("debug"),
        }

        // Add binary name
        path.join(binary_name)
    }

    /// Validate that the binary exists after build
    async fn validate_binary_exists(&self, binary_path: &Path) -> anyhow::Result<()> {
        if !binary_path.exists() {
            anyhow::bail!(
                "Expected binary not found at {:?}. Build may have completed but binary is missing. \
                This could indicate a build configuration issue.",
                binary_path
            );
        }
        Ok(())
    }

    /// Copy the binary to the output location
    async fn copy_binary(&self, binary_path: &Path, temp_path: &Path) -> anyhow::Result<()> {
        let output_path = temp_path.join("bootstrap");

        // Ensure parent directory exists
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create output directory: {:?}", parent))?;
        }

        // Copy the binary
        fs::copy(binary_path, &output_path).await.with_context(|| {
            format!(
                "Failed to copy binary from {:?} to {:?}",
                binary_path, output_path
            )
        })?;

        Ok(())
    }
}

/// Get cargo metadata for a project
async fn get_metadata(project_path: &Path) -> anyhow::Result<CargoMetadata> {
    let output = Command::new("cargo")
        .arg("metadata")
        .arg("--no-deps")
        .arg("--format-version=1")
        .current_dir(project_path)
        .output()
        .await
        .with_context(|| {
            format!(
                "Failed to run cargo metadata in directory: {:?}",
                project_path
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("cargo metadata failed: {}", stderr);
    }

    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)
        .context("Failed to parse cargo metadata. The output may be corrupted.")?;

    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_build_builder() {
        let build = RustBuild::new()
            .target("aarch64-unknown-linux-gnu".to_string())
            .profile(BuildProfile::Debug)
            .package_name("my-app")
            .binary_name("my-binary");

        assert_eq!(build.target, Some("aarch64-unknown-linux-gnu".to_string()));
        assert!(matches!(build.profile, BuildProfile::Debug));
        assert_eq!(build.package_name, Some("my-app".to_string()));
        assert_eq!(build.binary_name, Some("my-binary".to_string()));
    }

    #[test]
    fn test_default_rust_build() {
        let build = RustBuild::default();
        assert_eq!(build.target, Some("x86_64-unknown-linux-musl".to_string()));
        assert!(matches!(build.profile, BuildProfile::Release));
        assert_eq!(build.package_name, None);
        assert_eq!(build.binary_name, None);
    }
}
