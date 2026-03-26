use crate::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MediaConfig {
    pub source: MediaSource,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MediaSource {
    pub name: String,
    pub description: String,
    pub paths: Vec<String>,
    pub file_filters: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub deletion: Option<DeletionConfig>,
    pub validation: Option<ValidationConfig>,
    pub pre_processing: Option<PreProcessingConfig>,
    pub post_processing: Option<PostProcessingConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeletionConfig {
    pub delete_imported_files: bool,
    pub extra_file_patterns: Vec<String>, // Glob patterns for extra files to delete
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ValidationConfig {
    pub enabled: bool,
    pub ffprobe_validation: Option<FfprobeConfig>,
    pub custom_commands: Vec<CustomValidationCommand>,
    pub skip_on_validation_failure: bool, // If true, skip invalid files; if false, fail the entire operation
    pub max_validation_time_seconds: Option<u64>, // Timeout for validation commands
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FfprobeConfig {
    pub enabled: bool,
    pub ffprobe_path: Option<String>, // Path to ffprobe binary, defaults to "ffprobe"
    pub file_patterns: Vec<String>, // File patterns to validate, e.g., ["*.mp4", "*.mov", "*.avi"]
    pub required_streams: Vec<String>, // e.g., ["video", "audio"] or ["video"]
    pub min_duration_seconds: Option<f64>, // Minimum duration for valid media
    pub max_duration_seconds: Option<f64>, // Maximum duration for valid media
    pub check_corruption: bool, // Run deeper corruption checks
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CustomValidationCommand {
    pub name: String,
    pub command: String, // Command to execute, with {file_path} placeholder
    pub args: Vec<String>, // Additional arguments, can contain {file_path} placeholder
    pub file_patterns: Vec<String>, // File patterns to validate, e.g., ["*.jpg", "*.png"]
    pub expected_exit_code: i32, // Expected exit code for success (usually 0)
    pub timeout_seconds: Option<u64>, // Per-command timeout
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PostProcessingConfig {
    pub enabled: bool,
    pub hooks: Vec<PostProcessingHook>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PostProcessingHook {
    pub name: String,
    pub command: String, // Command to execute
    pub args: Vec<String>, // Command arguments with template variable support
    pub working_directory: Option<String>, // Working directory for the command (supports templates)
    pub timeout_seconds: Option<u64>, // Command timeout
    pub run_on_success: bool, // Run hook only if backup was successful
    pub run_on_failure: bool, // Run hook only if backup failed
    pub environment: Option<std::collections::HashMap<String, String>>, // Environment variables (supports templates)
    pub continue_on_error: bool, // Whether to continue processing other hooks if this one fails
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PreProcessingConfig {
    pub enabled: bool,
    pub commands: Vec<PreProcessingCommand>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PreProcessingCommand {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub file_patterns: Vec<String>,
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HostConfig {
    pub destinations: HashMap<String, Destination>,
    pub remotes: Option<HashMap<String, RemoteConfig>>,
    pub rclone: Option<RcloneGlobalConfig>,
    pub security: Option<SecurityConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Destination {
    pub path: String,
    pub rclone_remote: String,
    pub name_template: String,
    pub processing: ProcessingConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProcessingConfig {
    pub flatten_folders: bool,
    pub directory_structure: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RemoteConfig {
    #[serde(rename = "type")]
    pub remote_type: String,
    #[serde(flatten)]
    pub options: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RcloneGlobalConfig {
    pub bandwidth_limit: Option<String>,
    pub transfers: Option<u32>,
    pub additional_flags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecurityConfig {
    pub verify_integrity: bool,
    pub require_confirmation_before_delete: bool,
    pub hash_collision_detection: Option<HashCollisionConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HashCollisionConfig {
    pub initial_read_size: Option<u64>,
    pub max_read_size: Option<u64>,
    pub collision_multiplier: Option<f64>,
    pub enable_progressive_hashing: bool,
}

#[derive(Debug, Clone)]
pub struct DetectedMedia {
    pub name: String,
    pub description: String,
    pub mount_path: PathBuf,
    pub config: MediaConfig,
}

impl MediaConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: MediaConfig = toml::from_str(&content)?;
        Ok(config)
    }
}

impl HostConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: HostConfig = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    pub fn get_destination(&self, name: &str) -> Option<&Destination> {
        self.destinations.get(name)
    }

    pub fn list_destinations(&self) -> Vec<&String> {
        self.destinations.keys().collect()
    }
}

impl Default for ProcessingConfig {
    fn default() -> Self {
        Self {
            flatten_folders: false,
            directory_structure: "{original_path}".to_string(),
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            verify_integrity: true,
            require_confirmation_before_delete: true,
            hash_collision_detection: Some(HashCollisionConfig::default()),
        }
    }
}

impl Default for HashCollisionConfig {
    fn default() -> Self {
        Self {
            initial_read_size: Some(1024 * 1024),  // 1MB
            max_read_size: Some(16 * 1024 * 1024), // 16MB
            collision_multiplier: Some(2.0),       // Double the read size on collision
            enable_progressive_hashing: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RcloneConfig {
    pub rclone_config_path: Option<String>,
    pub rclone_binary_path: Option<String>,
    pub rclone_options: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_config_defaults() {
        let processing_config = ProcessingConfig::default();
        assert!(!processing_config.flatten_folders);
        assert_eq!(processing_config.directory_structure, "{original_path}");

        let security_config = SecurityConfig::default();
        assert!(security_config.verify_integrity);
        assert!(security_config.require_confirmation_before_delete);
        assert!(security_config.hash_collision_detection.is_some());

        let hash_config = HashCollisionConfig::default();
        assert!(hash_config.enable_progressive_hashing);
        assert_eq!(hash_config.initial_read_size, Some(1024 * 1024));
        assert_eq!(hash_config.max_read_size, Some(16 * 1024 * 1024));
        assert_eq!(hash_config.collision_multiplier, Some(2.0));
    }

    #[test]
    fn test_host_config_serialization() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config_path = temp_dir.path().join("test_config.toml");

        let mut destinations = HashMap::new();
        destinations.insert(
            "test_dest".to_string(),
            Destination {
                path: "/backup/path".to_string(),
                rclone_remote: "remote:bucket".to_string(),
                name_template: "{media_name}_{date}".to_string(),
                processing: ProcessingConfig {
                    flatten_folders: true,
                    directory_structure: "{yyyy}/{mm}".to_string(),
                },
            },
        );

        let original_config = HostConfig {
            destinations,
            remotes: None,
            rclone: Some(RcloneGlobalConfig {
                bandwidth_limit: Some("10M".to_string()),
                transfers: Some(4),
                additional_flags: vec!["--progress".to_string()],
            }),
            security: Some(SecurityConfig::default()),
        };

        // Save and load the config
        original_config.save(&config_path)?;
        let loaded_config = HostConfig::load(&config_path)?;

        // Verify the loaded config matches
        assert_eq!(loaded_config.destinations.len(), 1);
        assert!(loaded_config.destinations.contains_key("test_dest"));

        let dest = loaded_config.get_destination("test_dest").unwrap();
        assert_eq!(dest.path, "/backup/path");
        assert_eq!(dest.rclone_remote, "remote:bucket");
        assert!(dest.processing.flatten_folders);

        assert!(loaded_config.rclone.is_some());
        let rclone = loaded_config.rclone.unwrap();
        assert_eq!(rclone.bandwidth_limit, Some("10M".to_string()));
        assert_eq!(rclone.transfers, Some(4));

        Ok(())
    }

    #[test]
    fn test_media_config_loading() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config_path = temp_dir.path().join("media_config.toml");

        let config_content = r#"
[source]
name = "Test Camera"
description = "My camera's media"
paths = ["DCIM/", "Pictures/"]
file_filters = ["*.jpg", "*.mp4", "*.mov"]
exclude_patterns = ["*.bak", "*.tmp"]

[source.deletion]
delete_imported_files = true
extra_file_patterns = ["*.log"]
"#;

        std::fs::write(&config_path, config_content)?;
        let config = MediaConfig::load(&config_path)?;

        assert_eq!(config.source.name, "Test Camera");
        assert_eq!(config.source.description, "My camera's media");
        assert_eq!(config.source.paths, vec!["DCIM/", "Pictures/"]);
        assert_eq!(config.source.file_filters, vec!["*.jpg", "*.mp4", "*.mov"]);
        assert_eq!(config.source.exclude_patterns, vec!["*.bak", "*.tmp"]);

        let deletion = config.source.deletion.unwrap();
        assert!(deletion.delete_imported_files);
        assert_eq!(deletion.extra_file_patterns, vec!["*.log"]);

        Ok(())
    }

    #[test]
    fn test_host_config_destination_management() {
        let mut destinations = HashMap::new();
        destinations.insert(
            "dest1".to_string(),
            Destination {
                path: "/path1".to_string(),
                rclone_remote: "remote1:".to_string(),
                name_template: "{original_name}".to_string(),
                processing: ProcessingConfig::default(),
            },
        );
        destinations.insert(
            "dest2".to_string(),
            Destination {
                path: "/path2".to_string(),
                rclone_remote: "remote2:".to_string(),
                name_template: "{media_name}_{original_name}".to_string(),
                processing: ProcessingConfig::default(),
            },
        );

        let config = HostConfig {
            destinations,
            remotes: None,
            rclone: None,
            security: None,
        };

        // Test destination retrieval
        assert!(config.get_destination("dest1").is_some());
        assert!(config.get_destination("dest2").is_some());
        assert!(config.get_destination("nonexistent").is_none());

        // Test destination listing
        let dest_names = config.list_destinations();
        assert_eq!(dest_names.len(), 2);
        assert!(dest_names.contains(&&"dest1".to_string()));
        assert!(dest_names.contains(&&"dest2".to_string()));
    }

    #[test]
    fn test_pre_processing_config_loading() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config_path = temp_dir.path().join("media_config.toml");

        let config_content = r#"
[source]
name = "Test Camera"
description = "Test"
paths = ["DCIM/"]
file_filters = ["*.MP4"]
exclude_patterns = []

[source.pre_processing]
enabled = true

[[source.pre_processing.commands]]
name = "join_dji"
command = "dji-joiner"
args = ["-i", "{input_dir}", "-o", "{output_dir}", "--disable-frame-analysis"]
file_patterns = ["*.MP4", "*.mp4"]
timeout_seconds = 600
"#;

        std::fs::write(&config_path, config_content)?;
        let config = MediaConfig::load(&config_path)?;

        let pre = config.source.pre_processing.unwrap();
        assert!(pre.enabled);
        assert_eq!(pre.commands.len(), 1);
        assert_eq!(pre.commands[0].name, "join_dji");
        assert_eq!(pre.commands[0].command, "dji-joiner");
        assert_eq!(pre.commands[0].file_patterns, vec!["*.MP4", "*.mp4"]);

        Ok(())
    }

    #[test]
    fn test_remote_config_deserialization() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config_path = temp_dir.path().join("remote_config.toml");

        let config_content = r#"
[destinations.backup]
path = "/backup"
rclone_remote = "s3:bucket"
name_template = "{original_name}"

[destinations.backup.processing]
flatten_folders = false
directory_structure = "{yyyy}/{mm}"

[remotes.s3]
type = "s3"
provider = "AWS"
access_key_id = "test_key"
secret_access_key = "test_secret"
region = "us-east-1"

[rclone]
bandwidth_limit = "50M"
transfers = 8
additional_flags = ["--progress", "--stats", "30s"]
"#;

        std::fs::write(&config_path, config_content)?;
        let config = HostConfig::load(&config_path)?;

        // Test destination parsing
        assert!(config.destinations.contains_key("backup"));
        let dest = config.get_destination("backup").unwrap();
        assert_eq!(dest.path, "/backup");
        assert_eq!(dest.rclone_remote, "s3:bucket");

        // Test remote parsing
        assert!(config.remotes.is_some());
        let remotes = config.remotes.unwrap();
        assert!(remotes.contains_key("s3"));
        let remote = &remotes["s3"];
        assert_eq!(remote.remote_type, "s3");
        assert!(remote.options.contains_key("provider"));

        // Test rclone config
        assert!(config.rclone.is_some());
        let rclone = config.rclone.unwrap();
        assert_eq!(rclone.bandwidth_limit, Some("50M".to_string()));
        assert_eq!(rclone.transfers, Some(8));
        assert_eq!(rclone.additional_flags.len(), 3);

        Ok(())
    }
}
