use crate::config::{Destination, DetectedMedia, HostConfig};
use crate::hooks::HookExecutor;
use crate::rclone::RcloneWrapper;
use crate::templates::TemplateProcessor;
use crate::validation::{MediaValidator, ValidationResult};
use crate::Result;
use dialoguer::Confirm;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::symlink;

#[cfg(windows)]
use std::os::windows::fs::{symlink_dir, symlink_file};

pub struct FileProcessor {
    rclone: RcloneWrapper,
    dry_run: bool,
}

#[derive(Debug)]
pub struct ProcessingResult {
    pub files_processed: u64,
    pub bytes_transferred: u64,
    pub errors: Vec<String>,
    pub skipped_files: Vec<String>,
    pub successfully_imported_files: Vec<PathBuf>,
    pub validation_results: HashMap<PathBuf, Vec<ValidationResult>>,
    pub invalid_files: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct FileMapping {
    pub source_path: PathBuf,
    pub destination_path: String,
    pub relative_source_path: String,
    pub processed_filename: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BackupLog {
    pub media_name: String,
    pub backup_timestamp: String,
    pub destination: String,
    pub backed_up_files: Vec<BackedUpFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackedUpFile {
    pub source_path: String,
    pub destination_path: String,
    pub processed_filename: String,
    pub backup_timestamp: String,
}

impl FileProcessor {
    pub fn new(host_config: &HostConfig, dry_run: bool) -> Self {
        let rclone = RcloneWrapper::new(host_config.rclone.clone());

        // Clean up any existing temporary directories from previous runs
        Self::cleanup_temp_directories();

        // Clean up partial files from destinations
        Self::cleanup_partial_files(host_config);

        Self { rclone, dry_run }
    }

    fn cleanup_temp_directories() {
        let temp_dir = std::env::temp_dir();

        if let Ok(entries) = std::fs::read_dir(&temp_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("mdump_") && path.is_dir() {
                        // Force removal of directory and all contents including symlinks
                        if let Err(e) = Self::force_remove_dir_all(&path) {
                            eprintln!(
                                "Warning: Could not clean up old temp directory {}: {}",
                                path.display(),
                                e
                            );
                        }
                    }
                }
            }
        }
    }

    fn force_remove_dir_all(path: &Path) -> Result<()> {
        if path.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let entry_path = entry.path();
                if entry_path.is_dir() {
                    Self::force_remove_dir_all(&entry_path)?;
                } else {
                    // Remove file or symlink
                    std::fs::remove_file(&entry_path)?;
                }
            }
            std::fs::remove_dir(path)?;
        }
        Ok(())
    }

    fn cleanup_partial_files(host_config: &HostConfig) {
        for destination in host_config.destinations.values() {
            let dest_path = Path::new(&destination.path);
            if dest_path.exists() {
                if let Err(e) = Self::remove_partial_files_recursive(dest_path) {
                    eprintln!(
                        "Warning: Could not clean up partial files in {}: {}",
                        dest_path.display(),
                        e
                    );
                }
            }
        }
    }

    fn remove_partial_files_recursive(path: &Path) -> Result<()> {
        if path.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let entry_path = entry.path();
                if entry_path.is_dir() {
                    Self::remove_partial_files_recursive(&entry_path)?;
                } else if let Some(name) = entry_path.file_name().and_then(|n| n.to_str()) {
                    if name.ends_with(".partial") {
                        if let Err(e) = std::fs::remove_file(&entry_path) {
                            eprintln!(
                                "Warning: Could not remove partial file {}: {}",
                                entry_path.display(),
                                e
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn process_media(
        &self,
        media: &DetectedMedia,
        destination: &Destination,
        host_config: &HostConfig,
    ) -> Result<ProcessingResult> {
        println!("🔄 Processing media: {}", media.name);
        println!(
            "📁 Destination: {} ({})",
            destination.path, destination.rclone_remote
        );

        // Create template processor
        let mut template_processor = TemplateProcessor::new(media, destination, host_config)?;

        // Collect all files to process
        let files_to_process = self.collect_files_to_process(media)?;

        if files_to_process.is_empty() {
            println!("⚠️  No files found to process");
            return Ok(ProcessingResult {
                files_processed: 0,
                bytes_transferred: 0,
                errors: vec!["No files found to process".to_string()],
                skipped_files: Vec::new(),
                successfully_imported_files: Vec::new(),
                validation_results: HashMap::new(),
                invalid_files: Vec::new(),
            });
        }

        println!("📊 Found {} files to process", files_to_process.len());

        // Validate files if validation is configured
        let (validated_files, validation_results, invalid_files) = self.validate_files(&files_to_process, media).await?;

        if validated_files.is_empty() {
            println!("⚠️  No valid files remaining after validation");
            return Ok(ProcessingResult {
                files_processed: 0,
                bytes_transferred: 0,
                errors: vec!["No valid files remaining after validation".to_string()],
                skipped_files: files_to_process.iter().map(|p| p.to_string_lossy().to_string()).collect(),
                successfully_imported_files: Vec::new(),
                validation_results,
                invalid_files,
            });
        }

        if validated_files.len() != files_to_process.len() {
            println!("📊 {} files passed validation ({} invalid files filtered out)", 
                validated_files.len(), 
                files_to_process.len() - validated_files.len()
            );
        }

        // Create file mappings based on processing configuration
        let file_mappings = self.create_file_mappings(
            &validated_files,
            media,
            destination,
            &mut template_processor,
        )?;

        if self.dry_run {
            return self.dry_run_preview(&file_mappings, destination).await;
        }

        // Process files
        let mut result = self
            .process_files(
                &file_mappings,
                destination,
            )
            .await?;

        // Include validation results
        result.validation_results = validation_results;
        result.invalid_files = invalid_files;

        // Save backup log if files were successfully processed
        if result.files_processed > 0 && !result.successfully_imported_files.is_empty() {
            if let Err(e) = self.save_backup_log(media, &destination.path, &file_mappings) {
                println!("⚠️  Warning: Could not save backup log: {}", e);
            }
        }

        // Execute post-processing hooks if configured
        if let Some(post_processing_config) = &media.config.source.post_processing {
            let hook_executor = HookExecutor::new(template_processor);
            match hook_executor.execute_post_processing_hooks(
                post_processing_config,
                media,
                &result,
                &destination.path,
            ).await {
                Ok(hook_summary) => {
                    if hook_summary.failed_hooks > 0 {
                        println!("⚠️  Some post-processing hooks failed, but backup was successful");
                    }
                }
                Err(e) => {
                    println!("⚠️  Error executing post-processing hooks: {}", e);
                }
            }
        }

        Ok(result)
    }

    pub fn collect_files_to_process(&self, media: &DetectedMedia) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();

        for path_str in &media.config.source.paths {
            let full_path = media.mount_path.join(path_str);
            if full_path.exists() {
                self.collect_files_recursive(&full_path, &mut files, media)?;
            }
        }

        Ok(files)
    }

    fn collect_files_recursive(
        &self,
        path: &Path,
        files: &mut Vec<PathBuf>,
        media: &DetectedMedia,
    ) -> Result<()> {
        if path.is_file() {
            if self.matches_filters(path, media)? {
                files.push(path.to_path_buf());
            }
        } else if path.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                self.collect_files_recursive(&entry.path(), files, media)?;
            }
        }
        Ok(())
    }

    fn matches_filters(&self, file_path: &Path, media: &DetectedMedia) -> Result<bool> {
        let file_name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        // Check exclude patterns first
        for pattern in &media.config.source.exclude_patterns {
            if self.matches_glob_pattern(file_name, pattern)? {
                return Ok(false);
            }
        }

        // If no file filters specified, include all files
        if media.config.source.file_filters.is_empty() {
            return Ok(true);
        }

        // Check include patterns
        for pattern in &media.config.source.file_filters {
            if self.matches_glob_pattern(file_name, pattern)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn matches_glob_pattern(&self, file_name: &str, pattern: &str) -> Result<bool> {
        crate::media::glob_pattern_matches(file_name, pattern)
    }

    pub fn create_file_mappings(
        &self,
        files: &[PathBuf],
        media: &DetectedMedia,
        destination: &Destination,
        template_processor: &mut TemplateProcessor,
    ) -> Result<Vec<FileMapping>> {
        let mut mappings = Vec::new();
        let mut used_filenames = std::collections::HashMap::new();

        for file_path in files {
            let relative_path = file_path
                .strip_prefix(&media.mount_path)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| {
                    file_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                });

            // Set file-specific variables in template processor
            template_processor.set_file_variables(file_path, &relative_path)?;

            let mut processed_filename =
                template_processor.process_filename_template(&destination.name_template)?;

            // Handle duplicate filenames by adding a counter
            let base_filename = processed_filename.clone();
            let mut counter = 1;
            while used_filenames.contains_key(&processed_filename) {
                if let Some(dot_pos) = base_filename.rfind('.') {
                    let (name, ext) = base_filename.split_at(dot_pos);
                    processed_filename = format!("{}_{}{}", name, counter, ext);
                } else {
                    processed_filename = format!("{}_{}", base_filename, counter);
                }
                counter += 1;
            }
            used_filenames.insert(processed_filename.clone(), true);

            let destination_path = if destination.processing.flatten_folders {
                // Flatten: put all files in the base directory
                let dir_path = template_processor
                    .process_directory_template(&destination.processing.directory_structure)?;
                format!(
                    "{}/{}/{}",
                    destination.path,
                    dir_path.display(),
                    processed_filename
                )
            } else {
                // Preserve structure using directory template (which can include {original_path})
                let dir_structure = template_processor
                    .process_directory_template(&destination.processing.directory_structure)?;
                format!(
                    "{}/{}/{}",
                    destination.path,
                    dir_structure.display(),
                    processed_filename
                )
            };

            mappings.push(FileMapping {
                source_path: file_path.clone(),
                destination_path: destination_path.replace("//", "/"), // Clean double slashes
                relative_source_path: relative_path,
                processed_filename,
            });
        }

        Ok(mappings)
    }

    async fn dry_run_preview(
        &self,
        file_mappings: &[FileMapping],
        destination: &Destination,
    ) -> Result<ProcessingResult> {
        println!("🔍 DRY RUN - Preview of operations:");
        println!("Remote: {}", destination.rclone_remote);
        println!(
            "Flatten folders: {}",
            destination.processing.flatten_folders
        );
        println!(
            "Directory structure: {}",
            destination.processing.directory_structure
        );
        println!();

        let mut total_size = 0u64;

        for (i, mapping) in file_mappings.iter().enumerate() {
            let file_size = std::fs::metadata(&mapping.source_path)
                .map(|m| m.len())
                .unwrap_or(0);
            total_size += file_size;

            println!(
                "{}. {} -> {}",
                i + 1,
                mapping.relative_source_path,
                mapping.destination_path
            );

            if i >= 9 && file_mappings.len() > 10 {
                println!("... and {} more files", file_mappings.len() - 10);
                break;
            }
        }

        println!();
        println!("📊 Summary:");
        println!("  Files to process: {}", file_mappings.len());
        println!(
            "  Total size: {:.2} MB",
            total_size as f64 / 1024.0 / 1024.0
        );

        Ok(ProcessingResult {
            files_processed: file_mappings.len() as u64,
            bytes_transferred: total_size,
            errors: Vec::new(),
            skipped_files: Vec::new(),
            successfully_imported_files: Vec::new(), // Dry run doesn't actually import
            validation_results: HashMap::new(),
            invalid_files: Vec::new(),
        })
    }

    async fn process_files(
        &self,
        file_mappings: &[FileMapping],
        destination: &Destination,
    ) -> Result<ProcessingResult> {
        let mut result = ProcessingResult {
            files_processed: 0,
            bytes_transferred: 0,
            errors: Vec::new(),
            skipped_files: Vec::new(),
            successfully_imported_files: Vec::new(),
            validation_results: HashMap::new(),
            invalid_files: Vec::new(),
        };

        if file_mappings.is_empty() {
            return Ok(result);
        }

        println!(
            "📦 Preparing to copy {} files in a single rclone operation...",
            file_mappings.len()
        );

        // Show file mappings preview (similar to dry-run but more concise)
        println!("📋 File mappings:");
        for (i, mapping) in file_mappings.iter().enumerate() {
            println!(
                "  {}. {} -> {}",
                i + 1,
                mapping.relative_source_path,
                mapping.processed_filename
            );
        }
        println!();

        // Create a temporary directory structure that mirrors our desired layout
        let temp_dir = std::env::temp_dir().join(format!("mdump_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir)?;

        // Create symbolic links in the temp directory to match our desired structure
        println!("🔗 Creating temporary symbolic link structure...");
        for mapping in file_mappings {
            let link_target = &mapping.source_path;

            // Extract the relative path within our destination structure
            let relative_dest = mapping
                .destination_path
                .strip_prefix(&destination.path)
                .unwrap_or(&mapping.destination_path)
                .trim_start_matches('/');

            let link_path = temp_dir.join(relative_dest);

            // Create parent directories
            if let Some(parent) = link_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            // Create symbolic link
            #[cfg(unix)]
            symlink(link_target, &link_path)?;

            #[cfg(windows)]
            {
                if link_target.is_file() {
                    symlink_file(link_target, &link_path)?;
                } else {
                    symlink_dir(link_target, &link_path)?;
                }
            }
        }

        println!("🚀 Starting bulk rclone copy operation...");

        // Now copy the entire temp directory structure with a single rclone call
        match self
            .rclone
            .copy_with_progress(&temp_dir, &destination.path, destination)
            .await
        {
            Ok(copy_result) => {
                result.files_processed = file_mappings.len() as u64;
                result.bytes_transferred = copy_result.bytes_transferred;
                result.errors = copy_result.errors;

                if copy_result.success {
                    println!("✅ Bulk copy completed successfully!");
                    // Record all source files as successfully imported
                    for mapping in file_mappings {
                        result
                            .successfully_imported_files
                            .push(mapping.source_path.clone());
                    }
                } else {
                    println!("❌ Bulk copy completed with errors");
                }
            }
            Err(e) => {
                result.errors.push(format!("Bulk copy failed: {}", e));
                for mapping in file_mappings {
                    result
                        .skipped_files
                        .push(mapping.relative_source_path.clone());
                }
            }
        }

        // Clean up temporary directory
        if let Err(e) = std::fs::remove_dir_all(&temp_dir) {
            println!("⚠️  Warning: Could not clean up temporary directory: {}", e);
        }

        Ok(result)
    }

    pub async fn verify_integrity(
        &self,
        mappings: &[FileMapping],
        destination: &Destination,
        host_config: &HostConfig,
    ) -> Result<bool> {
        let security_config = host_config.security.as_ref();
        let should_verify = security_config.map(|s| s.verify_integrity).unwrap_or(true);

        if !should_verify {
            println!("⏭️  Integrity verification disabled");
            return Ok(true);
        }

        println!("🔍 Verifying integrity of copied files...");

        // For now, verify the entire destination
        // In a more sophisticated implementation, you might verify individual files
        let source_dirs: Vec<_> = mappings
            .iter()
            .map(|m| m.source_path.parent().unwrap_or_else(|| Path::new("")))
            .collect();

        for source_dir in source_dirs.into_iter() {
            if source_dir.as_os_str().is_empty() {
                continue;
            }

            let dest_path = &destination.path;
            let verified = self
                .rclone
                .check_integrity(source_dir, dest_path, destination)
                .await?;

            if !verified {
                println!("❌ Integrity check failed for {}", source_dir.display());
                return Ok(false);
            }
        }

        println!("✅ All files verified successfully");
        Ok(true)
    }

    pub async fn confirm_and_delete_source(
        &self,
        media: &DetectedMedia,
        processing_result: &ProcessingResult,
        host_config: &HostConfig,
    ) -> Result<bool> {
        // Check if deletion is configured for this media
        let deletion_config = match &media.config.source.deletion {
            Some(config) => config,
            None => {
                println!(
                    "ℹ️  No deletion configuration found for media: {}",
                    media.name
                );
                return Ok(true);
            }
        };

        if !deletion_config.delete_imported_files && deletion_config.extra_file_patterns.is_empty()
        {
            println!("ℹ️  Deletion disabled for media: {}", media.name);
            return Ok(true);
        }

        let security_config = host_config.security.as_ref();
        let require_confirmation = security_config
            .map(|s| s.require_confirmation_before_delete)
            .unwrap_or(true);

        if require_confirmation {
            println!("\n⚠️  DELETION CONFIRMATION REQUIRED");
            println!("Media: {}", media.name);
            println!("Mount path: {}", media.mount_path.display());

            if deletion_config.delete_imported_files {
                println!(
                    "Will delete {} successfully imported files",
                    processing_result.successfully_imported_files.len()
                );
            }

            if !deletion_config.extra_file_patterns.is_empty() {
                println!(
                    "Will delete extra files matching patterns: {:?}",
                    deletion_config.extra_file_patterns
                );
            }

            println!("This action cannot be undone!");
            println!();

            let confirmed = Confirm::new()
                .with_prompt("Are you sure you want to delete these files?")
                .default(false)
                .interact()?;

            if !confirmed {
                println!("🛑 Deletion cancelled by user");
                return Ok(false);
            }
        }

        println!("🗑️  Deleting specified files...");
        let mut deleted_count = 0;
        let mut failed_count = 0;

        // Delete successfully imported files if configured
        if deletion_config.delete_imported_files {
            for file_path in &processing_result.successfully_imported_files {
                if file_path.exists() {
                    match tokio::fs::remove_file(file_path).await {
                        Ok(_) => {
                            deleted_count += 1;
                            println!("🗑️  Deleted: {}", file_path.display());
                        }
                        Err(e) => {
                            failed_count += 1;
                            println!("❌ Failed to delete {}: {}", file_path.display(), e);
                        }
                    }
                }
            }
        }

        // Delete extra files matching patterns if configured
        if !deletion_config.extra_file_patterns.is_empty() {
            for path_str in &media.config.source.paths {
                let full_path = media.mount_path.join(path_str);
                if full_path.exists() && full_path.is_dir() {
                    match self
                        .delete_extra_files(&full_path, &deletion_config.extra_file_patterns)
                        .await
                    {
                        Ok(extra_deleted) => deleted_count += extra_deleted,
                        Err(e) => {
                            println!(
                                "❌ Failed to delete extra files in {}: {}",
                                full_path.display(),
                                e
                            );
                            failed_count += 1;
                        }
                    }
                }
            }
        }

        if failed_count == 0 {
            println!("✅ Successfully deleted {} files", deleted_count);
            Ok(true)
        } else {
            println!(
                "⚠️  Deleted {} files, {} failed",
                deleted_count, failed_count
            );
            Ok(false)
        }
    }

    fn delete_extra_files<'a>(
        &'a self,
        directory: &'a Path,
        patterns: &'a [String],
    ) -> BoxFuture<'a, Result<u32>> {
        Box::pin(async move {
            let mut deleted_count = 0;

            if let Ok(entries) = tokio::fs::read_dir(directory).await {
                let mut entries = entries;
                while let Some(entry) = entries.next_entry().await? {
                    let entry_path = entry.path();

                    if entry_path.is_file() {
                        if let Some(file_name) = entry_path.file_name().and_then(|n| n.to_str()) {
                            // Check if file matches any of the patterns
                            for pattern in patterns {
                                if self.matches_glob_pattern(file_name, pattern)? {
                                    match tokio::fs::remove_file(&entry_path).await {
                                        Ok(_) => {
                                            deleted_count += 1;
                                            println!(
                                                "🗑️  Deleted extra file: {}",
                                                entry_path.display()
                                            );
                                            break; // Don't check other patterns
                                        }
                                        Err(e) => {
                                            println!(
                                                "❌ Failed to delete {}: {}",
                                                entry_path.display(),
                                                e
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    } else if entry_path.is_dir() {
                        // Recursively check subdirectories
                        deleted_count += self.delete_extra_files(&entry_path, patterns).await?;
                    }
                }
            }

            Ok(deleted_count)
        })
    }

    fn get_backup_log_path(&self, media: &DetectedMedia) -> PathBuf {
        let log_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("mdump")
            .join("logs");

        std::fs::create_dir_all(&log_dir).ok();
        log_dir.join(format!("{}.json", media.name.replace(" ", "_")))
    }

    pub fn save_backup_log(
        &self,
        media: &DetectedMedia,
        destination: &str,
        mappings: &[FileMapping],
    ) -> Result<()> {
        let log_path = self.get_backup_log_path(media);
        let timestamp = chrono::Utc::now()
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string();

        // Load existing log or create new one
        let mut logs = self.load_backup_logs(media).unwrap_or_default();

        let backed_up_files: Vec<BackedUpFile> = mappings
            .iter()
            .map(|mapping| BackedUpFile {
                source_path: mapping.source_path.to_string_lossy().to_string(),
                destination_path: mapping.destination_path.clone(),
                processed_filename: mapping.processed_filename.clone(),
                backup_timestamp: timestamp.clone(),
            })
            .collect();

        let new_log = BackupLog {
            media_name: media.name.clone(),
            backup_timestamp: timestamp,
            destination: destination.to_string(),
            backed_up_files,
        };

        logs.push(new_log);

        // Keep only the last 10 backup sessions
        if logs.len() > 10 {
            logs = logs.into_iter().rev().take(10).rev().collect();
        }

        let json_content = serde_json::to_string_pretty(&logs)?;
        std::fs::write(&log_path, json_content)?;
        println!("📝 Backup log saved to: {}", log_path.display());

        Ok(())
    }

    pub fn load_backup_logs(&self, media: &DetectedMedia) -> Result<Vec<BackupLog>> {
        let log_path = self.get_backup_log_path(media);

        if !log_path.exists() {
            return Ok(Vec::new());
        }

        let content = std::fs::read_to_string(&log_path)?;
        let logs: Vec<BackupLog> = serde_json::from_str(&content)?;
        Ok(logs)
    }

    pub async fn delete_backed_up_files(
        &self,
        media: &DetectedMedia,
        host_config: &HostConfig,
        destination_filter: Option<&str>,
        auto_mode: bool,
    ) -> Result<bool> {
        // Check if deletion is configured for this media
        let deletion_config = match &media.config.source.deletion {
            Some(config) => config,
            None => {
                println!(
                    "ℹ️  No deletion configuration found for media: {}",
                    media.name
                );
                return Ok(true);
            }
        };

        if !deletion_config.delete_imported_files {
            println!("ℹ️  File deletion disabled for media: {}", media.name);
            return Ok(true);
        }

        // Load backup logs
        let logs = match self.load_backup_logs(media) {
            Ok(logs) => logs,
            Err(e) => {
                println!("❌ Could not load backup logs: {}", e);
                return Ok(false);
            }
        };

        if logs.is_empty() {
            println!("ℹ️  No backup logs found for media: {}", media.name);
            return Ok(true);
        }

        // Filter by destination if specified
        let relevant_logs: Vec<_> = logs
            .iter()
            .filter(|log| destination_filter.is_none_or(|dest| log.destination == dest))
            .collect();

        if relevant_logs.is_empty() {
            println!("ℹ️  No backup logs found for the specified destination");
            return Ok(true);
        }

        // Collect all unique backed up files
        let mut files_to_delete = HashMap::new();
        for log in &relevant_logs {
            for file in &log.backed_up_files {
                let source_path = PathBuf::from(&file.source_path);
                files_to_delete.insert(source_path.clone(), file.clone());
            }
        }

        if files_to_delete.is_empty() {
            println!("ℹ️  No backed up files found to delete");
            return Ok(true);
        }

        let security_config = host_config.security.as_ref();
        let require_confirmation = !auto_mode
            && security_config
                .map(|s| s.require_confirmation_before_delete)
                .unwrap_or(true);

        if require_confirmation {
            println!("\n⚠️  DELETION CONFIRMATION REQUIRED");
            println!("Media: {}", media.name);
            println!("Mount path: {}", media.mount_path.display());
            println!("Will delete {} backed up files:", files_to_delete.len());

            for (i, (source_path, file)) in files_to_delete.iter().enumerate().take(10) {
                println!(
                    "  {}. {} (backed up to {})",
                    i + 1,
                    source_path.display(),
                    file.processed_filename
                );
            }

            if files_to_delete.len() > 10 {
                println!("  ... and {} more files", files_to_delete.len() - 10);
            }

            println!("This action cannot be undone!");
            println!();

            let confirmed = Confirm::new()
                .with_prompt("Are you sure you want to delete these backed up files?")
                .default(false)
                .interact()?;

            if !confirmed {
                println!("🛑 Deletion cancelled by user");
                return Ok(false);
            }
        }

        println!("🗑️  Deleting backed up files...");
        let mut deleted_count = 0;
        let mut failed_count = 0;

        for source_path in files_to_delete.keys() {
            if source_path.exists() {
                match tokio::fs::remove_file(source_path).await {
                    Ok(_) => {
                        deleted_count += 1;
                        println!("🗑️  Deleted: {}", source_path.display());
                    }
                    Err(e) => {
                        failed_count += 1;
                        println!("❌ Failed to delete {}: {}", source_path.display(), e);
                    }
                }
            }
        }

        if failed_count == 0 {
            println!("✅ Successfully deleted {} files", deleted_count);
            Ok(true)
        } else {
            println!(
                "⚠️  Deleted {} files, {} failed",
                deleted_count, failed_count
            );
            Ok(false)
        }
    }

    pub async fn delete_importable_files(
        &self,
        media: &DetectedMedia,
        host_config: &HostConfig,
        auto_mode: bool,
    ) -> Result<bool> {
        // Check if deletion is configured for this media
        let deletion_config = match &media.config.source.deletion {
            Some(config) => config,
            None => {
                println!(
                    "ℹ️  No deletion configuration found for media: {}",
                    media.name
                );
                return Ok(true);
            }
        };

        if !deletion_config.delete_imported_files {
            println!("ℹ️  File deletion disabled for media: {}", media.name);
            return Ok(true);
        }

        println!("⚠️  FORCE MODE: Will delete files based on media configuration, not backup logs");

        // Collect all files that would be imported based on media configuration
        let importable_files = self.collect_files_to_process(media)?;

        if importable_files.is_empty() {
            println!("ℹ️  No importable files found to delete");
            return Ok(true);
        }

        let security_config = host_config.security.as_ref();
        let require_confirmation = !auto_mode
            && security_config
                .map(|s| s.require_confirmation_before_delete)
                .unwrap_or(true);

        if require_confirmation {
            println!("\n🚨 FORCE DELETE CONFIRMATION REQUIRED 🚨");
            println!("⚠️  WARNING: This will delete ALL files that match the media configuration!");
            println!("⚠️  This does NOT verify that files have been backed up!");
            println!();
            println!("Media: {}", media.name);
            println!("Mount path: {}", media.mount_path.display());
            println!("File filters: {:?}", media.config.source.file_filters);
            if !media.config.source.exclude_patterns.is_empty() {
                println!(
                    "Exclude patterns: {:?}",
                    media.config.source.exclude_patterns
                );
            }
            println!();
            println!(
                "Will delete {} files that match the configuration:",
                importable_files.len()
            );

            for (i, file_path) in importable_files.iter().enumerate().take(10) {
                let relative_path = file_path
                    .strip_prefix(&media.mount_path)
                    .unwrap_or(file_path)
                    .display();
                println!("  {}. {}", i + 1, relative_path);
            }

            if importable_files.len() > 10 {
                println!("  ... and {} more files", importable_files.len() - 10);
            }

            println!();
            println!("🚨 THIS ACTION CANNOT BE UNDONE! 🚨");
            println!("🚨 FILES WILL BE DELETED WITHOUT BACKUP VERIFICATION! 🚨");
            println!();

            // First confirmation
            let first_confirm = Confirm::new()
                .with_prompt(
                    "Do you understand this will delete files WITHOUT verifying backups exist?",
                )
                .default(false)
                .interact()?;

            if !first_confirm {
                println!("🛑 Force deletion cancelled by user");
                return Ok(false);
            }

            // Second confirmation with exact count
            let second_confirm = Confirm::new()
                .with_prompt(format!(
                    "Are you absolutely sure you want to permanently delete these {} files?",
                    importable_files.len()
                ))
                .default(false)
                .interact()?;

            if !second_confirm {
                println!("🛑 Force deletion cancelled by user");
                return Ok(false);
            }

            // Final confirmation with typing requirement
            println!();
            println!(
                "Final confirmation: Type 'DELETE {} FILES' to proceed:",
                importable_files.len()
            );
            let required_text = format!("DELETE {} FILES", importable_files.len());
            let input: String = dialoguer::Input::new()
                .with_prompt("Enter confirmation text")
                .interact_text()?;

            if input != required_text {
                println!("🛑 Confirmation text does not match. Force deletion cancelled.");
                return Ok(false);
            }
        } else {
            println!(
                "⚠️  Auto mode enabled - skipping confirmation for force delete of {} files",
                importable_files.len()
            );
        }

        println!("🗑️  Force deleting importable files...");
        let mut deleted_count = 0;
        let mut failed_count = 0;

        for file_path in &importable_files {
            if file_path.exists() {
                match tokio::fs::remove_file(file_path).await {
                    Ok(_) => {
                        deleted_count += 1;
                        let relative_path = file_path
                            .strip_prefix(&media.mount_path)
                            .unwrap_or(file_path)
                            .display();
                        println!("🗑️  Force deleted: {}", relative_path);
                    }
                    Err(e) => {
                        failed_count += 1;
                        println!("❌ Failed to delete {}: {}", file_path.display(), e);
                    }
                }
            }
        }

        // Also delete extra files if configured
        if !deletion_config.extra_file_patterns.is_empty() {
            for path_str in &media.config.source.paths {
                let full_path = media.mount_path.join(path_str);
                if full_path.exists() && full_path.is_dir() {
                    match self
                        .delete_extra_files(&full_path, &deletion_config.extra_file_patterns)
                        .await
                    {
                        Ok(extra_deleted) => deleted_count += extra_deleted,
                        Err(e) => {
                            println!(
                                "❌ Failed to delete extra files in {}: {}",
                                full_path.display(),
                                e
                            );
                            failed_count += 1;
                        }
                    }
                }
            }
        }

        if failed_count == 0 {
            println!("✅ Successfully force deleted {} files", deleted_count);
            Ok(true)
        } else {
            println!(
                "⚠️  Force deleted {} files, {} failed",
                deleted_count, failed_count
            );
            Ok(false)
        }
    }

    async fn validate_files(
        &self,
        files: &[PathBuf],
        media: &DetectedMedia,
    ) -> Result<(Vec<PathBuf>, HashMap<PathBuf, Vec<ValidationResult>>, Vec<PathBuf>)> {
        // Check if validation is configured
        let validation_config = match &media.config.source.validation {
            Some(config) if config.enabled => config,
            _ => {
                // No validation configured, return all files as valid
                return Ok((files.to_vec(), HashMap::new(), Vec::new()));
            }
        };

        let validator = MediaValidator::new(validation_config.clone());
        
        println!("🔍 Validating {} files...", files.len());
        
        let mut valid_files = Vec::new();
        let mut invalid_files = Vec::new();
        let mut validation_results = HashMap::new();
        
        for file_path in files {
            print!("🔍 Validating {}... ", file_path.file_name().unwrap_or_default().to_string_lossy());
            
            let results = match validator.validate_file(file_path).await {
                Ok(results) => results,
                Err(e) => {
                    println!("❌ Validation error: {}", e);
                    if validation_config.skip_on_validation_failure {
                        invalid_files.push(file_path.clone());
                        continue;
                    } else {
                        return Err(e);
                    }
                }
            };

            // Check if any validator failed
            let all_valid = results.iter().all(|r| r.is_valid);
            
            if all_valid {
                println!("✅ Valid");
                valid_files.push(file_path.clone());
            } else {
                // Show validation failures
                let failed_validators: Vec<&ValidationResult> = results.iter().filter(|r| !r.is_valid).collect();
                println!("❌ Invalid ({})", 
                    failed_validators.iter()
                        .map(|v| format!("{}: {}", v.validator_name, 
                            v.error_message.as_deref().unwrap_or("unknown error")))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                
                if validation_config.skip_on_validation_failure {
                    invalid_files.push(file_path.clone());
                } else {
                    return Err(anyhow::anyhow!(
                        "File validation failed for {}: {}",
                        file_path.display(),
                        failed_validators[0].error_message.as_deref().unwrap_or("validation failed")
                    ));
                }
            }
            
            validation_results.insert(file_path.clone(), results);
        }

        if !invalid_files.is_empty() {
            println!("⚠️  {} files failed validation and will be skipped", invalid_files.len());
        }
        
        Ok((valid_files, validation_results, invalid_files))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MediaConfig, MediaSource, ProcessingConfig, SecurityConfig};
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn create_test_host_config() -> HostConfig {
        HostConfig {
            destinations: HashMap::new(),
            remotes: None,
            rclone: None,
            security: Some(SecurityConfig::default()),
        }
    }

    fn create_test_media(mount_path: PathBuf) -> DetectedMedia {
        let config = MediaConfig {
            source: MediaSource {
                name: "Test Media".to_string(),
                description: "Test Description".to_string(),
                paths: vec!["test/".to_string()],
                file_filters: vec!["*.txt".to_string(), "*.jpg".to_string()],
                exclude_patterns: vec!["*.bak".to_string()],
                deletion: None,
                validation: None,
                post_processing: None,
            },
        };

        DetectedMedia {
            name: "Test Media".to_string(),
            description: "Test Description".to_string(),
            mount_path,
            config,
        }
    }

    fn create_test_destination() -> Destination {
        Destination {
            path: "/backup/dest".to_string(),
            rclone_remote: "test_remote".to_string(),
            name_template: "{media_name}_{original_name}".to_string(),
            processing: ProcessingConfig {
                flatten_folders: false,
                directory_structure: "{yyyy}/{mm}".to_string(),
            },
        }
    }

    #[test]
    fn test_matches_glob_pattern() -> Result<()> {
        let host_config = create_test_host_config();
        let processor = FileProcessor::new(&host_config, false);

        // Test basic patterns
        assert!(processor.matches_glob_pattern("test.txt", "*.txt")?);
        assert!(processor.matches_glob_pattern("test.jpg", "*.jpg")?);
        assert!(!processor.matches_glob_pattern("test.png", "*.jpg")?);

        // Test question mark wildcard
        assert!(processor.matches_glob_pattern("test1.txt", "test?.txt")?);
        assert!(!processor.matches_glob_pattern("test12.txt", "test?.txt")?);

        // Test literal dots
        assert!(processor.matches_glob_pattern("test.backup.txt", "*.backup.txt")?);

        Ok(())
    }

    #[test]
    fn test_file_filtering() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let test_path = temp_dir.path().join("test");
        std::fs::create_dir_all(&test_path)?;

        // Create test files
        std::fs::write(test_path.join("photo1.jpg"), "fake jpg content")?;
        std::fs::write(test_path.join("photo2.txt"), "text content")?;
        std::fs::write(test_path.join("backup.bak"), "backup content")?;
        std::fs::write(test_path.join("readme.md"), "readme content")?;

        let media = create_test_media(temp_dir.path().to_path_buf());
        let host_config = create_test_host_config();
        let processor = FileProcessor::new(&host_config, false);

        // Test included files
        assert!(processor.matches_filters(&test_path.join("photo1.jpg"), &media)?);
        assert!(processor.matches_filters(&test_path.join("photo2.txt"), &media)?);

        // Test excluded files
        assert!(!processor.matches_filters(&test_path.join("backup.bak"), &media)?);

        // Test files not matching filter
        assert!(!processor.matches_filters(&test_path.join("readme.md"), &media)?);

        Ok(())
    }

    #[test]
    fn test_create_file_mappings() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let test_path = temp_dir.path().join("test");
        std::fs::create_dir_all(&test_path)?;

        // Create test files
        let file1 = test_path.join("photo1.jpg");
        let file2 = test_path.join("photo2.txt");
        std::fs::write(&file1, "fake jpg content")?;
        std::fs::write(&file2, "text content")?;

        let media = create_test_media(temp_dir.path().to_path_buf());
        let destination = create_test_destination();
        let host_config = create_test_host_config();
        let processor = FileProcessor::new(&host_config, false);

        let files = vec![file1, file2];

        let mut template_processor = TemplateProcessor::new(&media, &destination, &host_config)?;
        let mappings = processor.create_file_mappings(
            &files,
            &media,
            &destination,
            &mut template_processor,
        )?;

        assert_eq!(mappings.len(), 2);

        // Check that mappings contain expected structure
        for mapping in &mappings {
            assert!(mapping.destination_path.starts_with(&destination.path));
            assert!(!mapping.processed_filename.is_empty());
            assert!(!mapping.relative_source_path.is_empty());
        }

        Ok(())
    }

    #[test]
    fn test_duplicate_filename_handling() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let test_path = temp_dir.path().join("test");
        std::fs::create_dir_all(&test_path)?;
        std::fs::create_dir_all(&test_path.join("subdir"))?;

        // Create files that would have the same processed name
        let file1 = test_path.join("photo.jpg");
        let file2 = test_path.join("subdir").join("photo.jpg");
        std::fs::write(&file1, "content1")?;
        std::fs::write(&file2, "content2")?;

        let media = create_test_media(temp_dir.path().to_path_buf());
        let mut destination = create_test_destination();
        // Use a simple template that would create duplicates
        destination.name_template = "{original_name}".to_string();

        let host_config = create_test_host_config();
        let processor = FileProcessor::new(&host_config, false);

        let files = vec![file1, file2];

        let mut template_processor = TemplateProcessor::new(&media, &destination, &host_config)?;
        let mappings = processor.create_file_mappings(
            &files,
            &media,
            &destination,
            &mut template_processor,
        )?;

        assert_eq!(mappings.len(), 2);

        // Check that filenames are different (one should have a counter)
        let filenames: Vec<_> = mappings.iter().map(|m| &m.processed_filename).collect();
        assert_ne!(filenames[0], filenames[1]);

        // One should be "photo.jpg" and the other "photo_1.jpg"
        assert!(filenames.contains(&&"photo.jpg".to_string()));
        assert!(filenames.iter().any(|f| f.contains("photo_1")));

        Ok(())
    }

    #[test]
    fn test_collect_files_recursive() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let test_path = temp_dir.path().join("test");
        std::fs::create_dir_all(&test_path.join("subdir"))?;

        // Create test files
        std::fs::write(test_path.join("photo1.jpg"), "content1")?;
        std::fs::write(test_path.join("photo2.txt"), "content2")?;
        std::fs::write(test_path.join("backup.bak"), "backup")?;
        std::fs::write(test_path.join("subdir").join("photo3.jpg"), "content3")?;

        let media = create_test_media(temp_dir.path().to_path_buf());
        let host_config = create_test_host_config();
        let processor = FileProcessor::new(&host_config, false);

        let files = processor.collect_files_to_process(&media)?;

        // Should find 3 files (2 .jpg, 1 .txt) but not the .bak file
        assert_eq!(files.len(), 3);

        // Check that we found the expected files
        let file_names: Vec<_> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_str().unwrap())
            .collect();

        assert!(file_names.contains(&"photo1.jpg"));
        assert!(file_names.contains(&"photo2.txt"));
        assert!(file_names.contains(&"photo3.jpg"));
        assert!(!file_names.contains(&"backup.bak"));

        Ok(())
    }
}
