use crate::config::{HostConfig, RemoteConfig};
use crate::Result;
use std::collections::HashMap;
use tokio::process::Command as AsyncCommand;

pub struct RemoteManager;

impl Default for RemoteManager {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteManager {
    pub fn new() -> Self {
        Self
    }

    pub async fn list_remotes(&self) -> Result<Vec<String>> {
        let output = AsyncCommand::new("rclone")
            .arg("listremotes")
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to execute rclone command: {}", e))?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Failed to list remotes: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let remotes = String::from_utf8(output.stdout)?
            .lines()
            .map(|line| line.trim_end_matches(':').to_string())
            .filter(|line| !line.is_empty())
            .collect();

        Ok(remotes)
    }

    pub async fn test_remote(&self, remote: &str) -> Result<bool> {
        let output = AsyncCommand::new("rclone")
            .arg("lsd")
            .arg(format!("{}:", remote))
            .arg("--max-depth")
            .arg("1")
            .output()
            .await?;

        Ok(output.status.success())
    }

    pub async fn remote_exists(&self, remote: &str) -> Result<bool> {
        let existing_remotes = self.list_remotes().await?;
        Ok(existing_remotes.contains(&remote.to_string()))
    }

    pub async fn setup_missing_remotes(&self, host_config: &HostConfig) -> Result<()> {
        let existing_remotes = self.list_remotes().await?;

        // Collect all remotes referenced by destinations
        let mut required_remotes = HashMap::new();

        for (dest_name, destination) in &host_config.destinations {
            let remote_name = &destination.rclone_remote;

            if !existing_remotes.contains(remote_name) {
                if let Some(remotes_config) = &host_config.remotes {
                    if let Some(remote_config) = remotes_config.get(remote_name) {
                        required_remotes.insert(remote_name.clone(), remote_config.clone());
                    } else {
                        println!(
                            "Warning: Destination '{}' references remote '{}' but no configuration found",
                            dest_name, remote_name
                        );
                    }
                } else {
                    println!(
                        "Warning: Destination '{}' references remote '{}' but no remotes section in config",
                        dest_name, remote_name
                    );
                }
            }
        }

        if required_remotes.is_empty() {
            println!("✅ All required remotes are already configured");
            return Ok(());
        }

        println!(
            "🔧 Setting up {} missing remote(s):",
            required_remotes.len()
        );

        for (remote_name, remote_config) in required_remotes {
            println!("Setting up remote: {}", remote_name);
            self.create_remote(&remote_name, &remote_config).await?;
        }

        Ok(())
    }

    async fn create_remote(&self, name: &str, config: &RemoteConfig) -> Result<()> {
        let mut cmd = AsyncCommand::new("rclone");
        cmd.arg("config")
            .arg("create")
            .arg(name)
            .arg(&config.remote_type);

        // Add configuration options
        for (key, value) in &config.options {
            let value_str = match value {
                toml::Value::String(s) => s.clone(),
                toml::Value::Integer(i) => i.to_string(),
                toml::Value::Float(f) => f.to_string(),
                toml::Value::Boolean(b) => b.to_string(),
                _ => continue, // Skip complex types
            };

            cmd.arg(format!("{}={}", key, value_str));
        }

        let output = cmd.output().await?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Failed to create remote '{}': {}",
                name,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        println!("✅ Remote '{}' created successfully", name);
        Ok(())
    }

    pub async fn import_existing_config(&self) -> Result<()> {
        // Get rclone config file location
        let output = AsyncCommand::new("rclone")
            .arg("config")
            .arg("file")
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Failed to get rclone config file location: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let config_path = String::from_utf8(output.stdout)?.trim().to_string();
        println!("Rclone config file: {}", config_path);

        let existing_remotes = self.list_remotes().await?;

        if existing_remotes.is_empty() {
            println!("No existing remotes found to import");
            return Ok(());
        }

        println!("Found existing remotes:");
        for remote in &existing_remotes {
            println!("  - {}", remote);

            // Test the remote
            match self.test_remote(remote).await {
                Ok(true) => println!("    ✅ Accessible"),
                Ok(false) => println!("    ❌ Not accessible"),
                Err(e) => println!("    ⚠️  Error testing: {}", e),
            }
        }

        Ok(())
    }

    pub async fn get_remote_info(&self, remote: &str) -> Result<HashMap<String, String>> {
        let output = AsyncCommand::new("rclone")
            .arg("config")
            .arg("show")
            .arg(remote)
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Failed to get remote info for '{}': {}",
                remote,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let config_text = String::from_utf8(output.stdout)?;
        let mut info = HashMap::new();

        for line in config_text.lines() {
            if let Some((key, value)) = line.split_once(" = ") {
                info.insert(key.trim().to_string(), value.trim().to_string());
            }
        }

        Ok(info)
    }

    pub async fn validate_remote_for_destination(
        &self,
        remote: &str,
        _destination_path: &str,
    ) -> Result<bool> {
        // Test if we can create a directory structure in the remote
        let test_path = format!("{}:/.mdump_test", remote);

        let output = AsyncCommand::new("rclone")
            .arg("mkdir")
            .arg(&test_path)
            .output()
            .await?;

        if output.status.success() {
            // Clean up test directory
            let _ = AsyncCommand::new("rclone")
                .arg("rmdir")
                .arg(&test_path)
                .output()
                .await;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
