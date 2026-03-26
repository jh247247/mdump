use crate::config::{CustomValidationCommand, FfprobeConfig, ValidationConfig};
use crate::Result;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct ValidationResult {
    pub is_valid: bool,
    pub validator_name: String,
    pub error_message: Option<String>,
    pub duration_ms: u128,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug)]
pub struct MediaValidator {
    config: ValidationConfig,
}

impl MediaValidator {
    pub fn new(config: ValidationConfig) -> Self {
        Self { config }
    }

    fn file_matches_patterns(file_path: &Path, patterns: &[String]) -> bool {
        if patterns.is_empty() {
            return true; // No patterns means match all files
        }

        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        patterns.iter().any(|pattern| {
            // Lowercase both sides for case-insensitive matching, then delegate to the
            // shared glob helper which correctly escapes dots before expanding wildcards.
            crate::media::glob_pattern_matches(
                &file_name.to_lowercase(),
                &pattern.to_lowercase(),
            )
            .unwrap_or_default()
        })
    }

    pub async fn validate_file(&self, file_path: &Path) -> Result<Vec<ValidationResult>> {
        if !self.config.enabled {
            return Ok(vec![ValidationResult {
                is_valid: true,
                validator_name: "validation_disabled".to_string(),
                error_message: None,
                duration_ms: 0,
                metadata: None,
            }]);
        }

        let mut results = Vec::new();
        
        // Run ffprobe validation if configured and file matches patterns
        if let Some(ffprobe_config) = &self.config.ffprobe_validation {
            if ffprobe_config.enabled && Self::file_matches_patterns(file_path, &ffprobe_config.file_patterns) {
                let result = self.validate_with_ffprobe(file_path, ffprobe_config).await?;
                results.push(result);
            }
        }

        // Run custom validation commands for matching file patterns
        for custom_command in &self.config.custom_commands {
            if Self::file_matches_patterns(file_path, &custom_command.file_patterns) {
                let result = self.validate_with_custom_command(file_path, custom_command).await?;
                results.push(result);
            }
        }

        Ok(results)
    }

    async fn validate_with_ffprobe(
        &self,
        file_path: &Path,
        config: &FfprobeConfig,
    ) -> Result<ValidationResult> {
        let start_time = std::time::Instant::now();
        let ffprobe_path = config.ffprobe_path.as_deref().unwrap_or("ffprobe");

        let mut cmd = Command::new(ffprobe_path);
        cmd.args([
            "-v", "quiet",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
        ]);

        if config.check_corruption {
            cmd.args(["-show_error", "-f", "null", "-"]);
        }

        cmd.arg(file_path);

        let timeout_duration = self.config.max_validation_time_seconds
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(30));

        let output = match tokio::time::timeout(timeout_duration, cmd.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Ok(ValidationResult {
                    is_valid: false,
                    validator_name: "ffprobe".to_string(),
                    error_message: Some(format!("Failed to execute ffprobe: {}", e)),
                    duration_ms: start_time.elapsed().as_millis(),
                    metadata: None,
                });
            }
            Err(_) => {
                return Ok(ValidationResult {
                    is_valid: false,
                    validator_name: "ffprobe".to_string(),
                    error_message: Some("ffprobe validation timed out".to_string()),
                    duration_ms: start_time.elapsed().as_millis(),
                    metadata: None,
                });
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Ok(ValidationResult {
                is_valid: false,
                validator_name: "ffprobe".to_string(),
                error_message: Some(format!("ffprobe failed: {}", stderr)),
                duration_ms: start_time.elapsed().as_millis(),
                metadata: None,
            });
        }

        // Parse ffprobe output
        let stdout = String::from_utf8_lossy(&output.stdout);
        let json_data: Value = match serde_json::from_str(&stdout) {
            Ok(data) => data,
            Err(e) => {
                return Ok(ValidationResult {
                    is_valid: false,
                    validator_name: "ffprobe".to_string(),
                    error_message: Some(format!("Failed to parse ffprobe output: {}", e)),
                    duration_ms: start_time.elapsed().as_millis(),
                    metadata: None,
                });
            }
        };

        // Validate based on configuration
        let validation_result = self.validate_ffprobe_output(&json_data, config);

        Ok(ValidationResult {
            is_valid: validation_result.0,
            validator_name: "ffprobe".to_string(),
            error_message: validation_result.1,
            duration_ms: start_time.elapsed().as_millis(),
            metadata: Some(json_data),
        })
    }

    fn validate_ffprobe_output(&self, json_data: &Value, config: &FfprobeConfig) -> (bool, Option<String>) {
        // Check for required streams
        if let Some(streams) = json_data.get("streams").and_then(|s| s.as_array()) {
            for required_stream in &config.required_streams {
                let has_stream = streams.iter().any(|stream| {
                    stream.get("codec_type")
                        .and_then(|t| t.as_str())
                        .map(|t| t == required_stream)
                        .unwrap_or(false)
                });

                if !has_stream {
                    return (false, Some(format!("Missing required stream type: {}", required_stream)));
                }
            }
        } else {
            return (false, Some("No streams found in media file".to_string()));
        }

        // Check duration constraints
        if let Some(format_info) = json_data.get("format") {
            if let Some(duration_str) = format_info.get("duration").and_then(|d| d.as_str()) {
                if let Ok(duration) = duration_str.parse::<f64>() {
                    if let Some(min_duration) = config.min_duration_seconds {
                        if duration < min_duration {
                            return (false, Some(format!("Duration {} seconds is below minimum {}", duration, min_duration)));
                        }
                    }

                    if let Some(max_duration) = config.max_duration_seconds {
                        if duration > max_duration {
                            return (false, Some(format!("Duration {} seconds is above maximum {}", duration, max_duration)));
                        }
                    }
                }
            }
        }

        (true, None)
    }

    async fn validate_with_custom_command(
        &self,
        file_path: &Path,
        config: &CustomValidationCommand,
    ) -> Result<ValidationResult> {
        let start_time = std::time::Instant::now();

        // Replace {file_path} placeholder in command and args
        let file_path_str = file_path.to_string_lossy();
        let processed_args: Vec<String> = config.args.iter()
            .map(|arg| arg.replace("{file_path}", &file_path_str))
            .collect();

        let mut cmd = Command::new(&config.command);
        cmd.args(&processed_args);

        let timeout_duration = config.timeout_seconds
            .or(self.config.max_validation_time_seconds)
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(30));

        let output = match tokio::time::timeout(timeout_duration, cmd.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Ok(ValidationResult {
                    is_valid: false,
                    validator_name: config.name.clone(),
                    error_message: Some(format!("Failed to execute command '{}': {}", config.command, e)),
                    duration_ms: start_time.elapsed().as_millis(),
                    metadata: None,
                });
            }
            Err(_) => {
                return Ok(ValidationResult {
                    is_valid: false,
                    validator_name: config.name.clone(),
                    error_message: Some(format!("Custom command '{}' timed out", config.name)),
                    duration_ms: start_time.elapsed().as_millis(),
                    metadata: None,
                });
            }
        };

        let is_valid = output.status.code().unwrap_or(-1) == config.expected_exit_code;
        let error_message = if is_valid {
            None
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            Some(format!("Command '{}' failed. Exit code: {}, stderr: {}, stdout: {}", 
                config.name, 
                output.status.code().unwrap_or(-1),
                stderr.trim(),
                stdout.trim()
            ))
        };

        Ok(ValidationResult {
            is_valid,
            validator_name: config.name.clone(),
            error_message,
            duration_ms: start_time.elapsed().as_millis(),
            metadata: None,
        })
    }

    pub fn should_skip_on_failure(&self) -> bool {
        self.config.skip_on_validation_failure
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs::File;
    use std::io::Write;

    fn create_test_validation_config() -> ValidationConfig {
        ValidationConfig {
            enabled: true,
            ffprobe_validation: Some(FfprobeConfig {
                enabled: true,
                ffprobe_path: None,
                file_patterns: vec!["*.mp4".to_string(), "*.mov".to_string()],
                required_streams: vec!["video".to_string()],
                min_duration_seconds: Some(1.0),
                max_duration_seconds: Some(3600.0),
                check_corruption: false,
            }),
            custom_commands: vec![
                CustomValidationCommand {
                    name: "file_exists".to_string(),
                    command: "test".to_string(),
                    args: vec!["-f".to_string(), "{file_path}".to_string()],
                    file_patterns: vec!["*".to_string()], // Match all files
                    expected_exit_code: 0,
                    timeout_seconds: Some(5),
                }
            ],
            skip_on_validation_failure: true,
            max_validation_time_seconds: Some(60),
        }
    }

    #[tokio::test]
    async fn test_custom_validation_file_exists() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let test_file = temp_dir.path().join("test.txt");
        let mut file = File::create(&test_file)?;
        file.write_all(b"test content")?;

        let config = create_test_validation_config();
        let validator = MediaValidator::new(config);

        let results = validator.validate_file(&test_file).await?;
        
        // Should have results from both ffprobe (likely fail) and custom command (should pass)
        assert!(!results.is_empty());
        
        // Find the file_exists validator result
        let file_exists_result = results.iter()
            .find(|r| r.validator_name == "file_exists")
            .expect("Should have file_exists validation result");
        
        assert!(file_exists_result.is_valid);

        Ok(())
    }

    #[test]
    fn test_placeholder_replacement() {
        let config = CustomValidationCommand {
            name: "test".to_string(),
            command: "echo".to_string(),
            args: vec!["File is: {file_path}".to_string(), "End".to_string()],
            file_patterns: vec!["*.mp4".to_string()],
            expected_exit_code: 0,
            timeout_seconds: None,
        };

        let file_path = Path::new("/test/path/file.mp4");
        let file_path_str = file_path.to_string_lossy();
        let processed_args: Vec<String> = config.args.iter()
            .map(|arg| arg.replace("{file_path}", &file_path_str))
            .collect();

        assert_eq!(processed_args[0], "File is: /test/path/file.mp4");
        assert_eq!(processed_args[1], "End");
    }

    #[test]
    fn test_file_pattern_matching() {
        // Test video file patterns
        let video_patterns = vec!["*.mp4".to_string(), "*.mov".to_string(), "*.avi".to_string()];
        
        assert!(MediaValidator::file_matches_patterns(Path::new("video.mp4"), &video_patterns));
        assert!(MediaValidator::file_matches_patterns(Path::new("VIDEO.MP4"), &video_patterns)); // Case insensitive
        assert!(MediaValidator::file_matches_patterns(Path::new("family_vacation.mov"), &video_patterns));
        assert!(!MediaValidator::file_matches_patterns(Path::new("photo.jpg"), &video_patterns));

        // Test image file patterns
        let image_patterns = vec!["*.jpg".to_string(), "*.jpeg".to_string(), "*.png".to_string()];
        
        assert!(MediaValidator::file_matches_patterns(Path::new("photo.jpg"), &image_patterns));
        assert!(MediaValidator::file_matches_patterns(Path::new("PHOTO.JPG"), &image_patterns));
        assert!(MediaValidator::file_matches_patterns(Path::new("screenshot.png"), &image_patterns));
        assert!(!MediaValidator::file_matches_patterns(Path::new("video.mp4"), &image_patterns));

        // Test empty patterns (should match all)
        let empty_patterns = vec![];
        assert!(MediaValidator::file_matches_patterns(Path::new("any_file.xyz"), &empty_patterns));

        // Test wildcard pattern
        let all_patterns = vec!["*".to_string()];
        assert!(MediaValidator::file_matches_patterns(Path::new("any_file.xyz"), &all_patterns));
    }
}