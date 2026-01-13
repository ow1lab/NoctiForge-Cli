use anyhow::{Context, bail};
use serde::Deserialize;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::process::Command;
use tonic::async_trait;
use tracing::{debug, info, warn};

use super::BuildService;

/// Custom build configuration
///
/// # Security Warning
/// Custom build scripts execute arbitrary shell commands with full system access.
/// Only use trusted configuration files. The script runs with the same permissions
/// as the build process.
#[derive(Debug, Deserialize)]
pub struct CustomBuild {
    /// Shell script or command to execute
    /// The OUTPUT environment variable will contain the temp directory path
    script: String,

    /// Optional timeout in seconds (default: 300 seconds / 5 minutes)
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,

    /// Optional working directory override
    /// If not specified, uses the project_path
    #[serde(default)]
    working_directory: Option<String>,

    /// Shell to use (default: "sh" on Unix, "cmd" on Windows)
    #[serde(default = "default_shell")]
    shell: String,
}

fn default_timeout() -> u64 {
    300 // 5 minutes
}

fn default_shell() -> String {
    if cfg!(target_os = "windows") {
        "cmd".to_string()
    } else {
        "sh".to_string()
    }
}

impl CustomBuild {
    /// Validate the custom build configuration
    fn validate(&self) -> anyhow::Result<()> {
        // Check script is not empty
        if self.script.trim().is_empty() {
            bail!("Build script cannot be empty");
        }

        // Warn about potentially dangerous commands
        let dangerous_patterns = ["rm -rf /", "format", "del /f /s /q", "sudo"];

        for pattern in &dangerous_patterns {
            if self.script.contains(pattern) {
                warn!(
                    "Build script contains potentially dangerous command: '{}'. \
                    Please review the script carefully.",
                    pattern
                );
            }
        }

        // Validate timeout
        if self.timeout_seconds == 0 {
            bail!("Timeout must be greater than 0");
        }

        if self.timeout_seconds > 3600 {
            warn!(
                "Build timeout is very long ({} seconds / {} minutes). \
                Consider reducing it to prevent hung builds.",
                self.timeout_seconds,
                self.timeout_seconds / 60
            );
        }

        Ok(())
    }

    /// Get the shell command arguments for the current platform
    fn get_shell_args(&self) -> Vec<&str> {
        if cfg!(target_os = "windows") {
            vec!["/C"]
        } else {
            vec!["-c"]
        }
    }
}

#[async_trait]
impl BuildService for CustomBuild {
    async fn build(&self, project_path: PathBuf, temp_path: PathBuf) -> anyhow::Result<()> {
        // Validate configuration
        self.validate()
            .context("Invalid custom build configuration")?;

        info!("Starting custom build script");
        debug!("Script: {}", self.script);
        debug!("Timeout: {}s", self.timeout_seconds);

        // Validate paths
        if !project_path.exists() {
            bail!("Project path does not exist: {:?}", project_path);
        }

        // Ensure temp directory exists
        tokio::fs::create_dir_all(&temp_path)
            .await
            .with_context(|| format!("Failed to create temp directory: {:?}", temp_path))?;

        // Determine working directory
        let working_dir = if let Some(ref wd) = self.working_directory {
            let custom_wd = project_path.join(wd);
            if !custom_wd.exists() {
                bail!("Custom working directory does not exist: {:?}", custom_wd);
            }
            custom_wd
        } else {
            project_path
        };

        debug!("Working directory: {:?}", working_dir);
        debug!("Output directory (OUTPUT env): {:?}", temp_path);

        // Build command
        let mut cmd = Command::new(&self.shell);

        for arg in self.get_shell_args() {
            cmd.arg(arg);
        }

        cmd.arg(&self.script)
            .current_dir(&working_dir)
            .env("OUTPUT", &temp_path)
            .env("PROJECT_PATH", &working_dir)
            .env("TEMP_PATH", &temp_path)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true); // Ensure child is killed if this future is dropped

        // Execute with timeout
        let child = cmd.spawn().with_context(|| {
            format!("Failed to spawn build script using shell '{}'", self.shell)
        })?;

        let timeout = Duration::from_secs(self.timeout_seconds);

        let status = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .with_context(|| {
                format!(
                    "Build script timed out after {} seconds. \
                    Consider increasing the timeout or optimizing your build.",
                    self.timeout_seconds
                )
            })?
            .with_context(|| "Failed to wait for build script completion")?
            .status;

        if !status.success() {
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".to_string());

            bail!(
                "Build script failed with exit code: {}. \
                Check the script output above for details.",
                code
            );
        }

        info!("Custom build script completed successfully");

        // Validate that something was produced
        let output_exists = tokio::fs::read_dir(&temp_path)
            .await
            .context("Failed to read output directory")?
            .next_entry()
            .await
            .context("Failed to check output directory contents")?
            .is_some();

        if !output_exists {
            warn!(
                "Build completed but output directory is empty. \
                Make sure your script writes to $OUTPUT"
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_empty_script() {
        let build = CustomBuild {
            script: "   ".to_string(),
            timeout_seconds: 300,
            working_directory: None,
            shell: default_shell(),
        };

        assert!(build.validate().is_err());
    }

    #[test]
    fn test_validate_zero_timeout() {
        let build = CustomBuild {
            script: "echo test".to_string(),
            timeout_seconds: 0,
            working_directory: None,
            shell: default_shell(),
        };

        assert!(build.validate().is_err());
    }

    #[test]
    fn test_validate_valid_config() {
        let build = CustomBuild {
            script: "echo 'Building...'".to_string(),
            timeout_seconds: 300,
            working_directory: None,
            shell: default_shell(),
        };

        assert!(build.validate().is_ok());
    }

    #[test]
    fn test_shell_args_unix() {
        let build = CustomBuild {
            script: "test".to_string(),
            timeout_seconds: 300,
            working_directory: None,
            shell: "sh".to_string(),
        };

        if !cfg!(target_os = "windows") {
            assert_eq!(build.get_shell_args(), vec!["-c"]);
        }
    }

    #[tokio::test]
    async fn test_simple_build() {
        let temp_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();

        let build = CustomBuild {
            script: "echo 'test content' > $OUTPUT/test.txt".to_string(),
            timeout_seconds: 10,
            working_directory: None,
            shell: default_shell(),
        };

        let result = build
            .build(
                project_dir.path().to_path_buf(),
                temp_dir.path().to_path_buf(),
            )
            .await;

        assert!(result.is_ok());

        let output_file = temp_dir.path().join("test.txt");
        assert!(output_file.exists());
    }
}
