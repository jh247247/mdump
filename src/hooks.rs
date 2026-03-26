use crate::config::{DetectedMedia, PostProcessingConfig, PostProcessingHook};
use crate::operations::ProcessingResult;
use crate::templates::TemplateProcessor;
use crate::Result;
use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

pub struct HookExecutor {
    template_processor: TemplateProcessor,
}

#[derive(Debug)]
pub struct HookResult {
    pub hook_name: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
    pub duration: Duration,
}

#[derive(Debug)]
pub struct HookExecutionSummary {
    pub total_hooks: usize,
    pub successful_hooks: usize,
    pub failed_hooks: usize,
    pub results: Vec<HookResult>,
}

impl HookExecutor {
    pub fn new(template_processor: TemplateProcessor) -> Self {
        Self { template_processor }
    }

    pub async fn execute_post_processing_hooks(
        &self,
        config: &PostProcessingConfig,
        media: &DetectedMedia,
        processing_result: &ProcessingResult,
        destination_path: &str,
    ) -> Result<HookExecutionSummary> {
        if !config.enabled || config.hooks.is_empty() {
            return Ok(HookExecutionSummary {
                total_hooks: 0,
                successful_hooks: 0,
                failed_hooks: 0,
                results: Vec::new(),
            });
        }

        let backup_succeeded = processing_result.errors.is_empty() && processing_result.files_processed > 0;

        println!("🔗 Executing {} post-processing hooks...", config.hooks.len());

        let mut results = Vec::new();
        let mut successful_hooks = 0;
        let mut failed_hooks = 0;

        for hook in &config.hooks {
            // Check if hook should run based on backup success/failure
            let should_run = (hook.run_on_success && backup_succeeded)
                || (hook.run_on_failure && !backup_succeeded)
                || (!hook.run_on_success && !hook.run_on_failure); // Run always if not specified

            if !should_run {
                println!("⏭️  Skipping hook '{}' (backup {}, hook conditions not met)",
                    hook.name,
                    if backup_succeeded { "succeeded" } else { "failed" }
                );
                continue;
            }

            println!("🎣 Executing hook: {}", hook.name);

            let start_time = std::time::Instant::now();
            let result = self.execute_hook(hook, media, processing_result, destination_path).await;
            let duration = start_time.elapsed();

            match result {
                Ok(hook_result) => {
                    if hook_result.success {
                        successful_hooks += 1;
                        println!("✅ Hook '{}' completed successfully ({}ms)",
                            hook.name, duration.as_millis());
                    } else {
                        failed_hooks += 1;
                        println!("❌ Hook '{}' failed with exit code {} ({}ms)",
                            hook.name,
                            hook_result.exit_code.unwrap_or(-1),
                            duration.as_millis());
                        if !hook_result.stderr.is_empty() {
                            println!("   stderr: {}", hook_result.stderr.trim());
                        }
                    }
                    results.push(hook_result);
                }
                Err(e) => {
                    failed_hooks += 1;
                    println!("❌ Hook '{}' failed with error: {}", hook.name, e);
                    results.push(HookResult {
                        hook_name: hook.name.clone(),
                        success: false,
                        exit_code: None,
                        stdout: String::new(),
                        stderr: String::new(),
                        error: Some(e.to_string()),
                        duration,
                    });
                }
            }

            // Stop executing hooks if this one failed and continue_on_error is false
            if !hook.continue_on_error && !results.last().unwrap().success {
                println!("🛑 Stopping hook execution due to failure (continue_on_error=false)");
                break;
            }
        }

        let summary = HookExecutionSummary {
            total_hooks: results.len(),
            successful_hooks,
            failed_hooks,
            results,
        };

        if summary.failed_hooks > 0 {
            println!("⚠️  Post-processing hooks completed: {}/{} successful, {} failed",
                summary.successful_hooks, summary.total_hooks, summary.failed_hooks);
        } else {
            println!("✅ All {} post-processing hooks completed successfully",
                summary.successful_hooks);
        }

        Ok(summary)
    }

    async fn execute_hook(
        &self,
        hook: &PostProcessingHook,
        media: &DetectedMedia,
        processing_result: &ProcessingResult,
        destination_path: &str,
    ) -> Result<HookResult> {
        // Process template variables in command arguments
        let processed_args = self.process_hook_arguments(hook, media, processing_result, destination_path)?;

        // Process working directory template
        let working_dir = if let Some(wd) = &hook.working_directory {
            Some(self.process_template_string(wd, media, processing_result, destination_path)?)
        } else {
            None
        };

        // Process environment variables
        let env_vars = if let Some(env) = &hook.environment {
            let mut processed_env = HashMap::new();
            for (key, value) in env {
                let processed_value = self.process_template_string(value, media, processing_result, destination_path)?;
                processed_env.insert(key.clone(), processed_value);
            }
            Some(processed_env)
        } else {
            None
        };

        // Create command
        let mut cmd = Command::new(&hook.command);
        cmd.args(&processed_args)
           .stdout(Stdio::piped())
           .stderr(Stdio::piped());

        if let Some(wd) = working_dir {
            cmd.current_dir(wd);
        }

        if let Some(env) = env_vars {
            for (key, value) in env {
                cmd.env(key, value);
            }
        }

        // Execute with timeout
        let timeout_duration = Duration::from_secs(hook.timeout_seconds.unwrap_or(300)); // Default 5 minutes

        let start_time = std::time::Instant::now();
        let output = match timeout(timeout_duration, cmd.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Ok(HookResult {
                    hook_name: hook.name.clone(),
                    success: false,
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(format!("Command execution failed: {}", e)),
                    duration: start_time.elapsed(),
                });
            }
            Err(_) => {
                return Ok(HookResult {
                    hook_name: hook.name.clone(),
                    success: false,
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some("Command timed out".to_string()),
                    duration: start_time.elapsed(),
                });
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code();
        let success = output.status.success();

        Ok(HookResult {
            hook_name: hook.name.clone(),
            success,
            exit_code,
            stdout,
            stderr,
            error: None,
            duration: start_time.elapsed(),
        })
    }

    fn process_hook_arguments(
        &self,
        hook: &PostProcessingHook,
        media: &DetectedMedia,
        processing_result: &ProcessingResult,
        destination_path: &str,
    ) -> Result<Vec<String>> {
        let mut processed_args = Vec::new();

        for arg in &hook.args {
            let processed = self.process_template_string(arg, media, processing_result, destination_path)?;
            processed_args.push(processed);
        }

        Ok(processed_args)
    }

    fn process_template_string(
        &self,
        template: &str,
        media: &DetectedMedia,
        processing_result: &ProcessingResult,
        destination_path: &str,
    ) -> Result<String> {
        let mut result = template.to_string();

        // Replace hook-specific variables
        result = result.replace("{media_name}", &media.name);
        result = result.replace("{media_description}", &media.description);
        result = result.replace("{mount_path}", &media.mount_path.to_string_lossy());
        result = result.replace("{destination_path}", destination_path);
        result = result.replace("{files_processed}", &processing_result.files_processed.to_string());
        result = result.replace("{bytes_transferred}", &processing_result.bytes_transferred.to_string());
        result = result.replace("{backup_success}", &processing_result.errors.is_empty().to_string());
        result = result.replace("{error_count}", &processing_result.errors.len().to_string());

        // Add timestamp variables
        let now = chrono::Utc::now();
        result = result.replace("{timestamp}", &now.format("%Y-%m-%d %H:%M:%S UTC").to_string());
        result = result.replace("{timestamp_iso}", &now.to_rfc3339());
        result = result.replace("{date}", &now.format("%Y-%m-%d").to_string());
        result = result.replace("{time}", &now.format("%H:%M:%S").to_string());

        // Process any template processor variables (for consistency with existing system)
        // This allows hooks to use the same template variables as file naming
        if let Ok(processed) = self.template_processor.process_simple_template(&result) {
            result = processed;
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Destination, HostConfig, MediaConfig, MediaSource, ProcessingConfig};
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn create_test_media() -> DetectedMedia {
        let temp_dir = TempDir::new().unwrap();
        DetectedMedia {
            name: "Test Media".to_string(),
            description: "Test Description".to_string(),
            mount_path: temp_dir.path().to_path_buf(),
            config: MediaConfig {
                source: MediaSource {
                    name: "Test Media".to_string(),
                    description: "Test Description".to_string(),
                    paths: vec!["test/".to_string()],
                    file_filters: vec!["*.jpg".to_string()],
                    exclude_patterns: vec![],
                    deletion: None,
                    validation: None,
                    post_processing: None,
                },
            },
        }
    }

    fn create_test_destination() -> Destination {
        Destination {
            path: "/test/backup".to_string(),
            rclone_remote: "test:".to_string(),
            name_template: "{original_name}".to_string(),
            processing: ProcessingConfig {
                flatten_folders: false,
                directory_structure: "{yyyy}/{mm}".to_string(),
            },
        }
    }

    fn create_test_template_processor() -> TemplateProcessor {
        let media = create_test_media();
        let destination = create_test_destination();
        let host_config = HostConfig {
            destinations: HashMap::new(),
            remotes: None,
            rclone: None,
            security: None,
        };
        TemplateProcessor::new(&media, &destination, &host_config).unwrap()
    }

    #[test]
    fn test_template_variable_processing() {
        let executor = HookExecutor::new(create_test_template_processor());
        let media = create_test_media();
        let processing_result = ProcessingResult {
            files_processed: 5,
            bytes_transferred: 1024000,
            errors: vec![],
            skipped_files: vec![],
            successfully_imported_files: vec![],
            validation_results: HashMap::new(),
            invalid_files: vec![],
        };

        let template = "Processed {files_processed} files from {media_name} to {destination_path}";
        let result = executor.process_template_string(
            template,
            &media,
            &processing_result,
            "/backup/dest"
        ).unwrap();

        assert!(result.contains("Processed 5 files"));
        assert!(result.contains("Test Media"));
        assert!(result.contains("/backup/dest"));
    }

    #[tokio::test]
    async fn test_simple_hook_execution() {
        let executor = HookExecutor::new(create_test_template_processor());
        let media = create_test_media();
        let processing_result = ProcessingResult {
            files_processed: 3,
            bytes_transferred: 512000,
            errors: vec![],
            skipped_files: vec![],
            successfully_imported_files: vec![],
            validation_results: HashMap::new(),
            invalid_files: vec![],
        };

        let hook = PostProcessingHook {
            name: "echo_test".to_string(),
            command: "echo".to_string(),
            args: vec!["Backup completed for {media_name}".to_string()],
            working_directory: None,
            timeout_seconds: Some(10),
            run_on_success: true,
            run_on_failure: false,
            environment: None,
            continue_on_error: true,
        };

        let result = executor.execute_hook(&hook, &media, &processing_result, "/test/backup").await.unwrap();

        assert!(result.success);
        assert_eq!(result.hook_name, "echo_test");
        assert!(result.stdout.contains("Test Media"));
    }

    #[tokio::test]
    async fn test_hook_execution_with_environment_variables() {
        let executor = HookExecutor::new(create_test_template_processor());
        let media = create_test_media();
        let processing_result = ProcessingResult {
            files_processed: 2,
            bytes_transferred: 256000,
            errors: vec![],
            skipped_files: vec![],
            successfully_imported_files: vec![],
            validation_results: HashMap::new(),
            invalid_files: vec![],
        };

        let mut env_vars = HashMap::new();
        env_vars.insert("BACKUP_MEDIA".to_string(), "{media_name}".to_string());
        env_vars.insert("BACKUP_FILES".to_string(), "{files_processed}".to_string());

        let hook = PostProcessingHook {
            name: "env_test".to_string(),
            command: "printenv".to_string(),
            args: vec!["BACKUP_MEDIA".to_string()],
            working_directory: None,
            timeout_seconds: Some(10),
            run_on_success: true,
            run_on_failure: false,
            environment: Some(env_vars),
            continue_on_error: true,
        };

        let result = executor.execute_hook(&hook, &media, &processing_result, "/test/backup").await.unwrap();

        assert!(result.success);
        assert!(result.stdout.contains("Test Media"));
    }
}