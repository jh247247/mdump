use crate::config::PreProcessingConfig;
use crate::Result;
use anyhow::bail;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Result of the pre-processing stage.
#[derive(Debug)]
pub struct PreProcessingResult {
    /// Files that should be backed up (staged output files + pass-through files).
    pub files_to_backup: Vec<PathBuf>,
    /// The original source files on the SD card / media that were matched and processed.
    pub original_source_files: Vec<PathBuf>,
    /// The temporary directory holding staged input/output files. Kept alive until
    /// the caller drops this struct so that `files_to_backup` remain accessible.
    pub staging_dir: Option<tempfile::TempDir>,
    /// Whether any pre-processing was actually executed.
    pub was_processed: bool,
}

/// Stateless helper that runs the pre-processing pipeline.
pub struct PreProcessor;

impl PreProcessor {
    /// Partition `files` into (matched, unmatched) according to the given glob patterns.
    /// Matching is case-insensitive, consistent with the validation system.
    pub fn partition_files(
        files: &[PathBuf],
        file_patterns: &[String],
    ) -> (Vec<PathBuf>, Vec<PathBuf>) {
        if file_patterns.is_empty() {
            // No patterns means match everything
            return (files.to_vec(), Vec::new());
        }

        let mut matched = Vec::new();
        let mut unmatched = Vec::new();

        for file in files {
            let file_name = file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();

            let is_match = file_patterns.iter().any(|pattern| {
                crate::media::glob_pattern_matches(&file_name, &pattern.to_lowercase())
                    .unwrap_or(false)
            });

            if is_match {
                matched.push(file.clone());
            } else {
                unmatched.push(file.clone());
            }
        }

        (matched, unmatched)
    }

    /// Main entry point. Runs the configured pre-processing command against matching
    /// files, stages them in a temporary directory, and returns the result.
    pub async fn run(
        config: &PreProcessingConfig,
        files: &[PathBuf],
        dry_run: bool,
    ) -> Result<PreProcessingResult> {
        // Nothing to do if disabled or no commands configured.
        if !config.enabled || config.commands.is_empty() {
            return Ok(PreProcessingResult {
                files_to_backup: files.to_vec(),
                original_source_files: Vec::new(),
                staging_dir: None,
                was_processed: false,
            });
        }

        // Only the first command is executed; warn if more are configured.
        if config.commands.len() > 1 {
            eprintln!(
                "Warning: {} pre-processing commands configured, but only the first ('{}') will be executed.",
                config.commands.len(),
                config.commands[0].name
            );
        }

        let command_config = &config.commands[0];

        // Partition files by the command's file patterns.
        let (matched_files, pass_through_files) =
            Self::partition_files(files, &command_config.file_patterns);

        if matched_files.is_empty() {
            return Ok(PreProcessingResult {
                files_to_backup: files.to_vec(),
                original_source_files: Vec::new(),
                staging_dir: None,
                was_processed: false,
            });
        }

        println!(
            "🔄 Pre-processing '{}': {} of {} files match patterns",
            command_config.name,
            matched_files.len(),
            files.len()
        );

        if dry_run {
            println!("🔍 DRY RUN: would run '{}' on:", command_config.name);
            for f in &matched_files {
                println!("   - {}", f.file_name().unwrap_or_default().to_string_lossy());
            }
            return Ok(PreProcessingResult {
                files_to_backup: files.to_vec(),
                original_source_files: Vec::new(),
                staging_dir: None,
                was_processed: false,
            });
        }

        // Create a temporary staging directory for output (and optionally input).
        let staging_dir = tempfile::Builder::new()
            .prefix("mdump_preprocess_")
            .tempdir()?;
        let output_dir = staging_dir.path().join("output");
        std::fs::create_dir_all(&output_dir)?;

        // Determine {input_dir}: either copy files to staging or read from source directly.
        let input_dir = if command_config.copy_input {
            let input_dir = staging_dir.path().join("input");
            std::fs::create_dir_all(&input_dir)?;

            println!("📋 Staging {} files for pre-processing...", matched_files.len());
            for source_file in &matched_files {
                let file_name = source_file
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");

                let dest_path = input_dir.join(file_name);

                // Handle filename collisions by prefixing the immediate parent directory name.
                let final_dest = if dest_path.exists() {
                    let parent_name = source_file
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown");
                    input_dir.join(format!("{}_{}", parent_name, file_name))
                } else {
                    dest_path
                };

                std::fs::copy(source_file, &final_dest)?;

                // Preserve original modification time.
                if let Ok(src_meta) = std::fs::metadata(source_file) {
                    if let Ok(mtime) = src_meta.modified() {
                        let ft = filetime::FileTime::from_system_time(mtime);
                        if let Err(e) = filetime::set_file_mtime(&final_dest, ft) {
                            eprintln!(
                                "Warning: could not preserve mtime for {}: {}",
                                final_dest.display(),
                                e
                            );
                        }
                    }
                }
            }
            input_dir
        } else {
            // Read directly from the source — find the common parent of matched files.
            let source_dir = matched_files[0]
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf();
            println!("📂 Reading directly from source: {}", source_dir.display());
            source_dir
        };

        println!("🚀 Running pre-processing command '{}'...", command_config.name);

        // Build command args with {input_dir} and {output_dir} substitution.
        let input_dir_str = input_dir.to_string_lossy();
        let output_dir_str = output_dir.to_string_lossy();

        let processed_args: Vec<String> = command_config
            .args
            .iter()
            .map(|arg| {
                arg.replace("{input_dir}", &input_dir_str)
                    .replace("{output_dir}", &output_dir_str)
            })
            .collect();

        // Execute the command with piped stdio so we can capture output.
        let mut cmd = Command::new(&command_config.command);
        cmd.args(&processed_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let timeout_duration = Duration::from_secs(command_config.timeout_seconds.unwrap_or(300));

        let output = match tokio::time::timeout(timeout_duration, cmd.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                bail!(
                    "Failed to execute pre-processing command '{}': {}",
                    command_config.command,
                    e
                );
            }
            Err(_) => {
                bail!(
                    "Pre-processing command '{}' timed out after {}s",
                    command_config.name,
                    timeout_duration.as_secs()
                );
            }
        };

        // Print stdout if non-empty (command progress / informational output).
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.trim().is_empty() {
            for line in stdout.trim().lines() {
                println!("   {}", line);
            }
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "Pre-processing command '{}' failed (exit code {}): {}",
                command_config.name,
                output.status.code().unwrap_or(-1),
                stderr.trim()
            );
        }

        // Collect all files produced in the output directory.
        let mut output_files = Vec::new();
        Self::collect_output_files(&output_dir, &mut output_files)?;

        println!(
            "✅ Pre-processing complete: {} output files (from {} inputs, {} pass-through)",
            output_files.len(),
            matched_files.len(),
            pass_through_files.len()
        );
        for f in &output_files {
            println!("   → {}", f.file_name().unwrap_or_default().to_string_lossy());
        }

        // Determine which matched source files were NOT consumed by the command.
        // If copy_input=false, the command reads from source directly — any source file
        // whose name doesn't appear in the output was not processed and should pass through.
        let mut consumed_source_files = Vec::new();
        let mut unconsumed_source_files = Vec::new();

        if !command_config.copy_input {
            let output_names: std::collections::HashSet<String> = output_files.iter()
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_lowercase())
                .collect();

            for source in &matched_files {
                let name = source.file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default();

                if output_names.contains(&name) {
                    // Command copied this file to output — use the output version
                    consumed_source_files.push(source.clone());
                } else {
                    // Command didn't produce this file — pass through the original
                    unconsumed_source_files.push(source.clone());
                }
            }

            if !unconsumed_source_files.is_empty() {
                println!("📂 {} source files passed through unchanged",
                         unconsumed_source_files.len());
            }
        } else {
            // With copy_input=true, all matched files were staged — treat all as consumed
            consumed_source_files = matched_files.clone();
        }

        // Combine: command output + unconsumed source originals + pattern-unmatched pass-throughs
        let mut files_to_backup = output_files;
        files_to_backup.extend_from_slice(&unconsumed_source_files);
        files_to_backup.extend_from_slice(&pass_through_files);

        Ok(PreProcessingResult {
            files_to_backup,
            original_source_files: consumed_source_files,
            staging_dir: Some(staging_dir),
            was_processed: true,
        })
    }

    /// Recursively collect all files under `dir` into `files`.
    fn collect_output_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                Self::collect_output_files(&path, files)?;
            } else {
                files.push(path);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PreProcessingCommand, PreProcessingConfig};
    use tempfile::TempDir;

    fn make_config(enabled: bool, commands: Vec<PreProcessingCommand>) -> PreProcessingConfig {
        PreProcessingConfig { enabled, commands }
    }

    fn make_command(
        name: &str,
        command: &str,
        args: Vec<&str>,
        patterns: Vec<&str>,
    ) -> PreProcessingCommand {
        PreProcessingCommand {
            name: name.to_string(),
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            file_patterns: patterns.iter().map(|s| s.to_string()).collect(),
            timeout_seconds: Some(30),
            copy_input: true,
        }
    }

    // Helper to create a real file on disk and return its PathBuf.
    fn create_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "fake content").unwrap();
        path
    }

    #[test]
    fn test_partition_files_matches_patterns() {
        let dir = TempDir::new().unwrap();
        let mp4 = create_file(dir.path(), "video.MP4");
        let lrf = create_file(dir.path(), "video.LRF");
        let jpg = create_file(dir.path(), "photo.jpg");

        let files = vec![mp4.clone(), lrf.clone(), jpg.clone()];
        let patterns = vec!["*.MP4".to_string()];

        let (matched, unmatched) = PreProcessor::partition_files(&files, &patterns);

        assert_eq!(matched, vec![mp4]);
        // LRF and jpg pass through
        assert!(unmatched.contains(&lrf));
        assert!(unmatched.contains(&jpg));
        assert_eq!(unmatched.len(), 2);
    }

    #[test]
    fn test_partition_files_case_insensitive() {
        let dir = TempDir::new().unwrap();
        let upper = create_file(dir.path(), "A.MP4");
        let lower = create_file(dir.path(), "b.mp4");
        let mixed = create_file(dir.path(), "C.Mp4");

        let files = vec![upper.clone(), lower.clone(), mixed.clone()];
        let patterns = vec!["*.MP4".to_string()];

        let (matched, unmatched) = PreProcessor::partition_files(&files, &patterns);

        assert_eq!(matched.len(), 3, "all three case variants should match *.MP4");
        assert!(unmatched.is_empty());
    }

    #[test]
    fn test_partition_files_no_matches() {
        let dir = TempDir::new().unwrap();
        let jpg = create_file(dir.path(), "photo.jpg");
        let pdf = create_file(dir.path(), "doc.pdf");

        let files = vec![jpg.clone(), pdf.clone()];
        let patterns = vec!["*.MP4".to_string()];

        let (matched, unmatched) = PreProcessor::partition_files(&files, &patterns);

        assert!(matched.is_empty());
        assert_eq!(unmatched.len(), 2);
    }

    #[tokio::test]
    async fn test_run_disabled_config() -> Result<()> {
        let dir = TempDir::new()?;
        let f1 = create_file(dir.path(), "video.mp4");
        let f2 = create_file(dir.path(), "photo.jpg");
        let files = vec![f1.clone(), f2.clone()];

        let config = make_config(false, vec![]);
        let result = PreProcessor::run(&config, &files, false).await?;

        assert!(!result.was_processed);
        assert_eq!(result.files_to_backup.len(), 2);
        assert!(result.staging_dir.is_none());
        assert!(result.original_source_files.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_run_dry_run_skips_execution() -> Result<()> {
        // `false` would always fail if actually run — dry_run must prevent execution.
        let dir = TempDir::new()?;
        let f = create_file(dir.path(), "video.mp4");
        let files = vec![f];

        let config = make_config(
            true,
            vec![make_command(
                "would_fail",
                "false",
                vec![],
                vec!["*.mp4"],
            )],
        );

        let result = PreProcessor::run(&config, &files, true).await?;

        assert!(!result.was_processed);
        assert!(result.staging_dir.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_run_with_cp_command() -> Result<()> {
        let dir = TempDir::new()?;
        let f = create_file(dir.path(), "clip.mp4");
        let files = vec![f];

        // cp -r {input_dir}/. {output_dir}/ copies everything from input to output.
        let config = make_config(
            true,
            vec![make_command(
                "copy_files",
                "cp",
                vec!["-r", "{input_dir}/.", "{output_dir}/"],
                vec!["*.mp4"],
            )],
        );

        let result = PreProcessor::run(&config, &files, false).await?;

        assert!(result.was_processed);
        assert!(!result.files_to_backup.is_empty(), "output dir should have files");
        // The staged output should contain the copied mp4.
        let has_mp4 = result
            .files_to_backup
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("mp4"));
        assert!(has_mp4, "output files should include the copied mp4");

        Ok(())
    }

    #[tokio::test]
    async fn test_run_pass_through_non_matching_files() -> Result<()> {
        let dir = TempDir::new()?;
        let mp4 = create_file(dir.path(), "clip.mp4");
        let jpg = create_file(dir.path(), "photo.jpg");
        let files = vec![mp4.clone(), jpg.clone()];

        // Use cp to copy matched MP4s; jpg should pass through with its original path.
        let config = make_config(
            true,
            vec![make_command(
                "copy_mp4",
                "cp",
                vec!["-r", "{input_dir}/.", "{output_dir}/"],
                vec!["*.mp4"],
            )],
        );

        let result = PreProcessor::run(&config, &files, false).await?;

        assert!(result.was_processed);

        // jpg should appear in files_to_backup with its original path.
        assert!(
            result.files_to_backup.contains(&jpg),
            "pass-through jpg should keep its original path"
        );

        // Should also have the processed mp4 output.
        let has_mp4 = result
            .files_to_backup
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("mp4"));
        assert!(has_mp4, "processed mp4 should appear in output");

        Ok(())
    }

    #[tokio::test]
    async fn test_run_command_failure() -> Result<()> {
        let dir = TempDir::new()?;
        let f = create_file(dir.path(), "clip.mp4");
        let files = vec![f];

        // `false` always exits with code 1.
        let config = make_config(
            true,
            vec![make_command("always_fail", "false", vec![], vec!["*.mp4"])],
        );

        let err = PreProcessor::run(&config, &files, false)
            .await
            .expect_err("should fail because 'false' exits non-zero");

        let msg = err.to_string();
        assert!(
            msg.contains("always_fail"),
            "error message should contain the command name; got: {msg}"
        );

        Ok(())
    }

    #[test]
    fn test_collect_output_files_recursive() -> Result<()> {
        let dir = TempDir::new()?;
        let sub = dir.path().join("subdir");
        std::fs::create_dir_all(&sub)?;

        create_file(dir.path(), "top_level.mp4");
        create_file(&sub, "nested.mp4");

        let mut files = Vec::new();
        PreProcessor::collect_output_files(dir.path(), &mut files)?;

        assert_eq!(files.len(), 2, "should collect files from dir and subdir");

        Ok(())
    }
}
