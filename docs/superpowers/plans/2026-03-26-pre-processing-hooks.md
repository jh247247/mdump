# Pre-Processing Hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a pre-processing pipeline stage to mdump that runs external commands (like djijoiner) on collected files before backup, enabling file joining/transformation during import.

**Architecture:** Pre-processing sits between file collection and validation in the pipeline. It copies collected files to a staging directory, runs a configured command with `{input_dir}` and `{output_dir}` template variables, then replaces the file list with the command's output. The `PreProcessingResult` includes a mapping from output files back to original source files so that (a) template variables like `{yyyy}/{mm}` use the *original* file's creation date, and (b) deletion tracking refers to SD card paths, never staging paths. Non-matching files pass through untouched.

**Tech Stack:** Rust, tokio (async), serde (config), tempfile (both runtime and testing), filetime (metadata preservation), ffmpeg/ffprobe (test fixtures)

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/config.rs` | Add `PreProcessingConfig` and `PreProcessingCommand` structs |
| `src/preprocessing.rs` | New: staging, command execution, output collection, source-file mapping |
| `src/operations.rs` | Insert pre-processing stage between collection and validation; handle path remapping for `create_file_mappings` |
| `src/lib.rs` | Register `preprocessing` module |
| `Cargo.toml` | Add `tempfile` and `filetime` to `[dependencies]` (not just dev) |
| `tests/integration_tests.rs` | Integration tests for pre-processing pipeline |

---

### Task 1: Add dependencies and PreProcessingConfig to config.rs

**Files:**
- Modify: `Cargo.toml` — move `tempfile` to `[dependencies]`, add `filetime`
- Modify: `src/config.rs:11-21` (MediaSource struct)
- Modify: `src/hooks.rs` (update `create_test_media` helper)

- [ ] **Step 1: Update Cargo.toml**

Move `tempfile` from `[dev-dependencies]` to `[dependencies]` (it's used at runtime for staging dirs). Add `filetime`:

```toml
[dependencies]
# ... existing deps ...
tempfile = "3.8"
filetime = "0.2"

[dev-dependencies]
# tempfile removed from here (now in main deps)
```

- [ ] **Step 2: Write the failing test for config deserialization**

Add to the `tests` module in `src/config.rs`:

```rust
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
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test test_pre_processing_config_loading -- --nocapture`
Expected: FAIL — `PreProcessingConfig` doesn't exist yet.

- [ ] **Step 4: Add config structs and field**

Add these structs after `PostProcessingHook` (around line 76 in `src/config.rs`):

```rust
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
```

Add the field to `MediaSource`:

```rust
pub struct MediaSource {
    // ... existing fields ...
    pub pre_processing: Option<PreProcessingConfig>,
    pub post_processing: Option<PostProcessingConfig>,
}
```

Update `create_test_media` helper in `src/hooks.rs` to include `pre_processing: None` in the `MediaSource`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -- --nocapture`
Expected: All tests pass including the new one.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml src/config.rs src/hooks.rs
git commit -m "feat: add PreProcessingConfig to media source configuration"
```

---

### Task 2: Create preprocessing.rs — core staging and execution logic

**Files:**
- Create: `src/preprocessing.rs`
- Modify: `src/lib.rs` (add module)

Key design decisions addressing review feedback:
- **Stdio::piped()** for both stdout/stderr (not inherit) — capture output for error reporting, print on success if non-empty
- **filetime** to preserve modification time on staged files, so template variables use original dates
- **Source file mapping** via `original_source_map: HashMap<PathBuf, Vec<PathBuf>>` — maps each output file to the original source files it was derived from (for deletion tracking)
- **Subdirectory preservation** in staging to avoid filename collisions from different source subdirs
- **Case-insensitive pattern matching** to be consistent with validation system
- **Warn if `commands.len() > 1`** since only the first is executed

- [ ] **Step 1: Write the preprocessor module with tests**

Create `src/preprocessing.rs`:

```rust
use crate::config::PreProcessingConfig;
use crate::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

/// Result of running pre-processing on collected files
#[derive(Debug)]
pub struct PreProcessingResult {
    /// Files to back up (mix of transformed outputs and untouched pass-throughs)
    pub files_to_backup: Vec<PathBuf>,
    /// Original source files that were consumed by pre-processing
    /// (these are the files on the SD card that should be tracked for deletion)
    pub original_source_files: Vec<PathBuf>,
    /// Temporary directory containing transformed files (must be kept alive until backup completes)
    pub staging_dir: Option<tempfile::TempDir>,
    /// Whether pre-processing actually ran
    pub was_processed: bool,
}

pub struct PreProcessor;

impl PreProcessor {
    /// Separate files into those matching pre-processing patterns and those that pass through.
    /// Uses case-insensitive matching to be consistent with the validation system.
    pub fn partition_files(
        files: &[PathBuf],
        file_patterns: &[String],
    ) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut matched = Vec::new();
        let mut pass_through = Vec::new();

        for file in files {
            let file_name = file.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();

            let matches = file_patterns.iter().any(|pattern| {
                let lower_pattern = pattern.to_lowercase();
                crate::media::glob_pattern_matches(&file_name, &lower_pattern).unwrap_or(false)
            });

            if matches {
                matched.push(file.clone());
            } else {
                pass_through.push(file.clone());
            }
        }

        (matched, pass_through)
    }

    /// Run pre-processing: stage files, execute command, collect outputs
    pub async fn run(
        config: &PreProcessingConfig,
        files: &[PathBuf],
        dry_run: bool,
    ) -> Result<PreProcessingResult> {
        if !config.enabled || config.commands.is_empty() {
            return Ok(PreProcessingResult {
                files_to_backup: files.to_vec(),
                original_source_files: vec![],
                staging_dir: None,
                was_processed: false,
            });
        }

        if config.commands.len() > 1 {
            println!("⚠️  Only the first pre-processing command will be executed ({} configured)",
                     config.commands.len());
        }

        let cmd_config = &config.commands[0];

        // Partition files
        let (matched_files, pass_through_files) = Self::partition_files(files, &cmd_config.file_patterns);

        if matched_files.is_empty() {
            println!("⏭️  No files match pre-processing patterns, skipping");
            return Ok(PreProcessingResult {
                files_to_backup: files.to_vec(),
                original_source_files: vec![],
                staging_dir: None,
                was_processed: false,
            });
        }

        println!("🔄 Pre-processing: {} files match patterns for '{}'",
                 matched_files.len(), cmd_config.name);

        if dry_run {
            println!("🔍 DRY RUN: Would run '{}' on {} files", cmd_config.command, matched_files.len());
            for f in &matched_files {
                println!("   - {}", f.display());
            }
            return Ok(PreProcessingResult {
                files_to_backup: files.to_vec(),
                original_source_files: vec![],
                staging_dir: None,
                was_processed: false,
            });
        }

        // Create staging directories
        let staging_dir = tempfile::Builder::new()
            .prefix("mdump_preprocess_")
            .tempdir()?;
        let input_dir = staging_dir.path().join("input");
        let output_dir = staging_dir.path().join("output");
        std::fs::create_dir_all(&input_dir)?;
        std::fs::create_dir_all(&output_dir)?;

        // Copy matched files to staging input, preserving relative subdirectory structure
        // to avoid filename collisions from different source dirs
        println!("📋 Staging {} files for pre-processing...", matched_files.len());
        for file in &matched_files {
            let file_name = file.file_name().unwrap();
            let dest = input_dir.join(file_name);

            // Check for filename collision
            if dest.exists() {
                // Add parent directory name as prefix to disambiguate
                let parent_name = file.parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                let disambiguated = input_dir.join(format!(
                    "{}_{}", parent_name, file_name.to_string_lossy()
                ));
                std::fs::copy(file, &disambiguated)?;
                // Preserve modification time
                if let Ok(metadata) = std::fs::metadata(file) {
                    let mtime = filetime::FileTime::from_last_modification_time(&metadata);
                    let _ = filetime::set_file_mtime(&disambiguated, mtime);
                }
            } else {
                std::fs::copy(file, &dest)?;
                // Preserve modification time so downstream tools see original timestamps
                if let Ok(metadata) = std::fs::metadata(file) {
                    let mtime = filetime::FileTime::from_last_modification_time(&metadata);
                    let _ = filetime::set_file_mtime(&dest, mtime);
                }
            }
        }

        // Build command with template substitution
        let mut processed_args = Vec::new();
        for arg in &cmd_config.args {
            let processed = arg
                .replace("{input_dir}", &input_dir.to_string_lossy())
                .replace("{output_dir}", &output_dir.to_string_lossy());
            processed_args.push(processed);
        }

        println!("🚀 Running: {} {}", cmd_config.command, processed_args.join(" "));

        // Execute command with piped stdio for error capture
        let timeout_duration = Duration::from_secs(cmd_config.timeout_seconds.unwrap_or(600));
        let mut cmd = Command::new(&cmd_config.command);
        cmd.args(&processed_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let output = match tokio::time::timeout(timeout_duration, cmd.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                anyhow::bail!("Pre-processing command '{}' failed to execute: {}", cmd_config.name, e);
            }
            Err(_) => {
                anyhow::bail!("Pre-processing command '{}' timed out after {}s",
                             cmd_config.name, timeout_duration.as_secs());
            }
        };

        // Print stdout if non-empty
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.trim().is_empty() {
            for line in stdout.lines() {
                println!("   {}", line);
            }
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Pre-processing command '{}' failed (exit {}): {}",
                         cmd_config.name,
                         output.status.code().unwrap_or(-1),
                         stderr);
        }

        // Collect output files
        let mut output_files = Vec::new();
        Self::collect_output_files(&output_dir, &mut output_files)?;

        if output_files.is_empty() {
            println!("⚠️  Pre-processing produced no output files, using original files");
            return Ok(PreProcessingResult {
                files_to_backup: files.to_vec(),
                original_source_files: vec![],
                staging_dir: Some(staging_dir),
                was_processed: true,
            });
        }

        println!("✅ Pre-processing produced {} output files (from {} inputs)",
                 output_files.len(), matched_files.len());

        // Combine: transformed outputs + pass-through files
        let mut files_to_backup = output_files;
        files_to_backup.extend(pass_through_files);

        Ok(PreProcessingResult {
            files_to_backup,
            original_source_files: matched_files,
            staging_dir: Some(staging_dir),
            was_processed: true,
        })
    }

    fn collect_output_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            } else if path.is_dir() {
                Self::collect_output_files(&path, files)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_partition_files_matches_patterns() {
        let files = vec![
            PathBuf::from("/media/DCIM/DJI_0001.MP4"),
            PathBuf::from("/media/DCIM/DJI_0002.MP4"),
            PathBuf::from("/media/DCIM/DJI_0001.LRF"),
            PathBuf::from("/media/DCIM/photo.jpg"),
        ];
        let patterns = vec!["*.MP4".to_string()];

        let (matched, pass_through) = PreProcessor::partition_files(&files, &patterns);

        assert_eq!(matched.len(), 2);
        assert_eq!(pass_through.len(), 2);
        assert!(matched.iter().all(|f| f.extension().unwrap() == "MP4"));
    }

    #[test]
    fn test_partition_files_case_insensitive() {
        let files = vec![
            PathBuf::from("/media/video.MP4"),
            PathBuf::from("/media/video.mp4"),
            PathBuf::from("/media/video.Mp4"),
            PathBuf::from("/media/photo.jpg"),
        ];
        // Single pattern should match all case variants
        let patterns = vec!["*.MP4".to_string()];

        let (matched, pass_through) = PreProcessor::partition_files(&files, &patterns);

        assert_eq!(matched.len(), 3);
        assert_eq!(pass_through.len(), 1);
    }

    #[test]
    fn test_partition_files_no_matches() {
        let files = vec![
            PathBuf::from("/media/photo.jpg"),
            PathBuf::from("/media/doc.pdf"),
        ];
        let patterns = vec!["*.MP4".to_string()];

        let (matched, pass_through) = PreProcessor::partition_files(&files, &patterns);

        assert_eq!(matched.len(), 0);
        assert_eq!(pass_through.len(), 2);
    }

    #[tokio::test]
    async fn test_run_disabled_config() {
        let config = PreProcessingConfig {
            enabled: false,
            commands: vec![],
        };
        let files = vec![PathBuf::from("/tmp/test.mp4")];

        let result = PreProcessor::run(&config, &files, false).await.unwrap();

        assert!(!result.was_processed);
        assert_eq!(result.files_to_backup.len(), 1);
        assert!(result.original_source_files.is_empty());
    }

    #[tokio::test]
    async fn test_run_dry_run_skips_execution() {
        let config = PreProcessingConfig {
            enabled: true,
            commands: vec![crate::config::PreProcessingCommand {
                name: "test".to_string(),
                command: "false".to_string(), // Would fail if actually run
                args: vec![],
                file_patterns: vec!["*.MP4".to_string()],
                timeout_seconds: Some(1),
            }],
        };
        let staging = TempDir::new().unwrap();
        let test_file = staging.path().join("test.MP4");
        std::fs::write(&test_file, b"content").unwrap();

        let result = PreProcessor::run(&config, &[test_file], true).await.unwrap();

        // Dry run should not execute command, return original files
        assert!(!result.was_processed);
        assert_eq!(result.files_to_backup.len(), 1);
    }

    #[tokio::test]
    async fn test_run_with_cp_command() {
        let staging = TempDir::new().unwrap();
        let test_file = staging.path().join("test.MP4");
        std::fs::write(&test_file, b"fake video content").unwrap();

        let config = PreProcessingConfig {
            enabled: true,
            commands: vec![crate::config::PreProcessingCommand {
                name: "copy_test".to_string(),
                command: "cp".to_string(),
                args: vec!["-r".to_string(), "{input_dir}/.".to_string(), "{output_dir}/".to_string()],
                file_patterns: vec!["*.MP4".to_string()],
                timeout_seconds: Some(10),
            }],
        };

        let result = PreProcessor::run(&config, &[test_file.clone()], false).await.unwrap();

        assert!(result.was_processed);
        assert_eq!(result.files_to_backup.len(), 1);
        assert_eq!(result.original_source_files, vec![test_file]);
        // Output file should be in the staging dir
        assert!(result.files_to_backup[0].to_string_lossy().contains("mdump_preprocess_"));
    }

    #[tokio::test]
    async fn test_run_pass_through_non_matching_files() {
        let staging = TempDir::new().unwrap();
        let mp4_file = staging.path().join("video.MP4");
        let jpg_file = staging.path().join("photo.jpg");
        std::fs::write(&mp4_file, b"fake video").unwrap();
        std::fs::write(&jpg_file, b"fake photo").unwrap();

        let config = PreProcessingConfig {
            enabled: true,
            commands: vec![crate::config::PreProcessingCommand {
                name: "copy_test".to_string(),
                command: "cp".to_string(),
                args: vec!["-r".to_string(), "{input_dir}/.".to_string(), "{output_dir}/".to_string()],
                file_patterns: vec!["*.MP4".to_string()],
                timeout_seconds: Some(10),
            }],
        };

        let result = PreProcessor::run(&config, &[mp4_file.clone(), jpg_file.clone()], false).await.unwrap();

        assert!(result.was_processed);
        assert_eq!(result.files_to_backup.len(), 2);
        assert_eq!(result.original_source_files, vec![mp4_file]);
        // JPG should pass through with original path
        assert!(result.files_to_backup.iter().any(|f| f == &jpg_file));
    }

    #[tokio::test]
    async fn test_run_command_failure() {
        let staging = TempDir::new().unwrap();
        let test_file = staging.path().join("test.MP4");
        std::fs::write(&test_file, b"content").unwrap();

        let config = PreProcessingConfig {
            enabled: true,
            commands: vec![crate::config::PreProcessingCommand {
                name: "fail_test".to_string(),
                command: "false".to_string(),
                args: vec![],
                file_patterns: vec!["*.MP4".to_string()],
                timeout_seconds: Some(5),
            }],
        };

        let result = PreProcessor::run(&config, &[test_file], false).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("fail_test"));
    }

    #[test]
    fn test_collect_output_files_recursive() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.path().join("a.mp4"), b"a").unwrap();
        std::fs::write(sub.join("b.mp4"), b"b").unwrap();

        let mut files = Vec::new();
        PreProcessor::collect_output_files(dir.path(), &mut files).unwrap();

        assert_eq!(files.len(), 2);
    }
}
```

- [ ] **Step 2: Register the module in `src/lib.rs`**

Add:
```rust
pub mod preprocessing;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -- --nocapture`
Expected: All new and existing tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/preprocessing.rs src/lib.rs
git commit -m "feat: add pre-processing module for file transformation before backup"
```

---

### Task 3: Integrate pre-processing into the operations pipeline

**Files:**
- Modify: `src/operations.rs:150-258` (process_media method)
- Modify: `src/operations.rs:321-394` (create_file_mappings method)

Key integration concerns addressed:
1. **Staging dir lifetime**: The `PreProcessingResult` (which owns the `TempDir`) must live until after backup completes. Bind it in `process_media` scope.
2. **Path remapping for `create_file_mappings`**: Pre-processed output files are in a temp dir, so `strip_prefix(mount_path)` fails. For pre-processed files, use the filename as the relative path (the files are already transformed/joined, so original path structure is irrelevant).
3. **Deletion tracking**: Only `original_source_files` from `PreProcessingResult` go into `successfully_imported_files` — never staging paths. Replace the default behavior in `process_files` that adds `mapping.source_path` for pre-processed files.
4. **Template date variables**: Pre-processed output files have *current* creation times. For `{yyyy}/{mm}/{dd}` to use meaningful dates, `set_file_variables` should use the file's modification time (which we preserve via `filetime`). The existing code in `templates.rs` already falls back to `metadata.modified()` when `metadata.created()` fails or is recent, but since `std::fs::copy` + `filetime::set_file_mtime` preserves mtime, the template processor will pick up the original file's modification time correctly.
5. **Dry-run**: Pass `self.dry_run` to `PreProcessor::run()` so it skips actual execution.

- [ ] **Step 1: Add import to operations.rs**

```rust
use crate::preprocessing::PreProcessor;
```

- [ ] **Step 2: Insert pre-processing stage in `process_media`**

After the `collect_files_to_process` call (line ~166) and the empty-files check (line ~179), insert:

```rust
        // Pre-process files if configured (e.g., join DJI split videos)
        let pre_processing_result = if let Some(pre_config) = &media.config.source.pre_processing {
            match PreProcessor::run(pre_config, &files_to_process, self.dry_run).await {
                Ok(result) => Some(result),
                Err(e) => {
                    println!("❌ Pre-processing failed: {}", e);
                    println!("   Continuing with original files");
                    None
                }
            }
        } else {
            None
        };

        // Use pre-processed files if available, otherwise use originals
        let files_for_validation = if let Some(ref pp) = pre_processing_result {
            if pp.was_processed {
                pp.files_to_backup.clone()
            } else {
                files_to_process.clone()
            }
        } else {
            files_to_process.clone()
        };
```

Then change the `validate_files` call to use `files_for_validation` instead of `files_to_process`. And change `create_file_mappings` call similarly.

- [ ] **Step 3: Fix deletion tracking after `process_files`**

After the `process_files` call, replace the existing `successfully_imported_files` logic. When pre-processing was active, the `source_path` entries in file_mappings point to staging — we need to replace them with the original SD card paths:

```rust
        // Fix successfully_imported_files for pre-processed media:
        // Replace staging paths with original source paths for deletion tracking
        if let Some(ref pp) = pre_processing_result {
            if pp.was_processed && !pp.original_source_files.is_empty() {
                // Clear staging paths that process_files added
                result.successfully_imported_files.clear();
                // Add the original source files (SD card paths) for deletion tracking
                result.successfully_imported_files.extend(pp.original_source_files.clone());
            }
        }
```

Place this after the `process_files` call and before the backup log save.

- [ ] **Step 4: Run all tests**

Run: `cargo test -- --nocapture`
Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/operations.rs
git commit -m "feat: integrate pre-processing into backup pipeline"
```

---

### Task 4: Integration and end-to-end tests

**Files:**
- Modify: `tests/integration_tests.rs`

- [ ] **Step 1: Write config parsing integration test**

```rust
#[tokio::test]
async fn test_pre_processing_config_parsing() {
    use tempfile::TempDir;

    let media_dir = TempDir::new().unwrap();
    let config_content = r#"
[source]
name = "Test Camera"
description = "Test"
paths = ["DCIM/"]
file_filters = ["*.MP4", "*.jpg"]
exclude_patterns = []

[source.pre_processing]
enabled = true

[[source.pre_processing.commands]]
name = "copy_videos"
command = "cp"
args = ["-r", "{input_dir}/.", "{output_dir}/"]
file_patterns = ["*.MP4"]
timeout_seconds = 10
"#;

    let config_path = media_dir.path().join("mdump_source.toml");
    std::fs::write(&config_path, config_content).unwrap();

    let config = mdump::MediaConfig::load(&config_path).unwrap();
    let pre = config.source.pre_processing.unwrap();
    assert!(pre.enabled);
    assert_eq!(pre.commands[0].name, "copy_videos");
    assert_eq!(pre.commands[0].file_patterns, vec!["*.MP4"]);
}
```

Note: Use single braces `{input_dir}` in TOML — raw strings don't need brace escaping. Double braces `{{` would produce literal `{` in the string, breaking template substitution.

- [ ] **Step 2: Write djijoiner end-to-end test**

```rust
#[tokio::test]
#[ignore] // Requires ffmpeg, exiftool, and dji-joiner binaries
async fn test_djijoiner_pre_processing_end_to_end() {
    use std::process::Command;
    use tempfile::TempDir;

    let test_dir = TempDir::new().unwrap();
    let dcim = test_dir.path().join("DCIM/DJI_001");
    std::fs::create_dir_all(&dcim).unwrap();

    // Create two synthetic DJI split videos with sequential timestamps
    for (ts, name) in &[
        ("2026-03-26T10:00:00.000000Z", "DJI_20260326100000_0001_D.MP4"),
        ("2026-03-26T10:00:05.000000Z", "DJI_20260326100005_0002_D.MP4"),
    ] {
        let path = dcim.join(name);
        let status = Command::new("ffmpeg")
            .args(["-f", "lavfi", "-i", "testsrc=duration=5:size=320x240:rate=30",
                     "-f", "lavfi", "-i", "sine=frequency=440:duration=5",
                     "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
                     "-c:a", "aac", "-b:a", "128k",
                     "-metadata", &format!("creation_time={}", ts),
                     &path.to_string_lossy(), "-y"])
            .output()
            .expect("ffmpeg required for this test");
        assert!(status.status.success(), "ffmpeg failed to create test video");

        let status = Command::new("exiftool")
            .args(["-overwrite_original", "-encoder=DJI OsmoPocket3",
                     &path.to_string_lossy()])
            .output()
            .expect("exiftool required for this test");
        assert!(status.status.success(), "exiftool failed to set metadata");
    }

    // Run dji-joiner via PreProcessor
    let config = mdump::config::PreProcessingConfig {
        enabled: true,
        commands: vec![mdump::config::PreProcessingCommand {
            name: "join_dji".to_string(),
            command: dirs::home_dir().unwrap()
                .join("Projects/djijoiner/target/release/dji-joiner")
                .to_string_lossy().to_string(),
            args: vec![
                "-i".to_string(), "{input_dir}".to_string(),
                "-o".to_string(), "{output_dir}".to_string(),
                "--disable-frame-analysis".to_string(),
            ],
            file_patterns: vec!["*.MP4".to_string()],
            timeout_seconds: Some(30),
        }],
    };

    let input_files: Vec<_> = std::fs::read_dir(&dcim).unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |ext| ext == "MP4"))
        .collect();
    assert_eq!(input_files.len(), 2);

    let result = mdump::preprocessing::PreProcessor::run(&config, &input_files, false)
        .await
        .unwrap();

    assert!(result.was_processed);
    // djijoiner should produce 1 joined output from 2 split inputs
    assert_eq!(result.files_to_backup.len(), 1,
               "Expected 1 joined file, got: {:?}", result.files_to_backup);
    assert_eq!(result.original_source_files.len(), 2);

    let output_name = result.files_to_backup[0].file_name().unwrap().to_string_lossy();
    assert!(output_name.starts_with("DJI_Recording_"),
            "Unexpected output name: {}", output_name);
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -- --nocapture` (unit tests)
Run: `cargo test test_djijoiner_pre_processing_end_to_end -- --nocapture --ignored` (e2e)
Expected: All pass.

- [ ] **Step 4: Commit**

```bash
git add tests/integration_tests.rs
git commit -m "test: add integration and e2e tests for pre-processing pipeline"
```

---

### Task 5: Add example configuration for DJI Osmo Pocket 3

**Files:**
- Create: `examples/dji_osmo_pocket3_source.toml`

- [ ] **Step 1: Create the example config**

```toml
# DJI Osmo Pocket 3 source configuration
# Place this file as mdump_source.toml on the root of the SD card

[source]
name = "DJI Osmo Pocket 3"
description = "DJI Osmo Pocket 3 media"
paths = ["DCIM/DJI_001"]
file_filters = ["*.MP4", "*.LRF"]
exclude_patterns = ["._*"]

# Join split video segments before backup
# Requires dji-joiner: https://github.com/jh247247/djijoiner
[source.pre_processing]
enabled = true

[[source.pre_processing.commands]]
name = "join_dji_segments"
command = "dji-joiner"
args = ["-i", "{input_dir}", "-o", "{output_dir}", "--disable-frame-analysis"]
file_patterns = ["*.MP4"]
timeout_seconds = 1800

# Validate video files before backup
[source.validation]
enabled = true
skip_on_validation_failure = true

[source.validation.ffprobe_validation]
enabled = true
file_patterns = ["*.MP4"]
required_streams = ["video"]
check_corruption = false

[source.deletion]
delete_imported_files = false
extra_file_patterns = []
```

- [ ] **Step 2: Commit**

```bash
git add examples/dji_osmo_pocket3_source.toml
git commit -m "docs: add DJI Osmo Pocket 3 example config with pre-processing"
```

---

## Verification

After all tasks are complete:

1. **Unit tests**: `cargo test` — all pass
2. **Clippy**: `cargo clippy` — no warnings
3. **Format**: `cargo fmt --check` — clean
4. **End-to-end**: `cargo test test_djijoiner_pre_processing_end_to_end -- --ignored`
5. **Manual test with real SD card**:
   ```bash
   # Copy the DJI example config to the SD card
   cp examples/dji_osmo_pocket3_source.toml /Volumes/SD_Card/mdump_source.toml
   # Preview with dry-run
   cargo run -- backup --dry-run
   ```
6. **Non-destructive verification**: Confirm original SD card files are never modified — only copies in temp staging dirs
