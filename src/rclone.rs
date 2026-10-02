use crate::config::{Destination, RcloneGlobalConfig};
use crate::Result;
use portable_pty::{CommandBuilder, PtySize};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use tokio::process::Command;

pub struct RcloneWrapper {
    global_config: Option<RcloneGlobalConfig>,
}

#[derive(Debug)]
pub struct CopyResult {
    pub success: bool,
    pub files_transferred: u64,
    pub bytes_transferred: u64,
    pub errors: Vec<String>,
}

impl RcloneWrapper {
    pub fn new(global_config: Option<RcloneGlobalConfig>) -> Self {
        Self { global_config }
    }

    fn analyze_file_sizes(source: &Path) -> Result<(u64, u64, u64)> {
        let mut total_files = 0;
        let mut total_size = 0;
        let mut large_files = 0;
        let large_file_threshold = 100 * 1024 * 1024; // 100MB threshold

        fn scan_directory(
            path: &Path,
            total_files: &mut u64,
            total_size: &mut u64,
            large_files: &mut u64,
            threshold: u64,
        ) -> Result<()> {
            if path.is_file() {
                let metadata = std::fs::metadata(path)?;
                let size = metadata.len();
                *total_files += 1;
                *total_size += size;
                if size > threshold {
                    *large_files += 1;
                }
            } else if path.is_dir() {
                for entry in std::fs::read_dir(path)? {
                    let entry = entry?;
                    scan_directory(
                        &entry.path(),
                        total_files,
                        total_size,
                        large_files,
                        threshold,
                    )?;
                }
            }
            Ok(())
        }

        scan_directory(
            source,
            &mut total_files,
            &mut total_size,
            &mut large_files,
            large_file_threshold,
        )?;
        Ok((total_files, total_size, large_files))
    }

    fn calculate_optimal_transfers(
        total_files: u64,
        total_size: u64,
        large_files: u64,
        cpu_count: u32,
    ) -> (u32, u32, u32) {
        let large_file_ratio = if total_files > 0 {
            (large_files as f64) / (total_files as f64)
        } else {
            0.0
        };

        let avg_file_size = if total_files > 0 {
            total_size / total_files
        } else {
            0
        };

        // If more than 50% are large files or average file size > 50MB, use conservative settings
        if large_file_ratio > 0.5 || avg_file_size > 50 * 1024 * 1024 {
            println!("📊 Large files detected ({}% large, avg {:.1}MB) - using conservative transfer settings",
                (large_file_ratio * 100.0) as u32,
                avg_file_size as f64 / 1024.0 / 1024.0
            );

            // Conservative settings for large files
            let transfers = 4u32.min(cpu_count);
            let checkers = transfers * 2; // More checkers than transfers for large files
            let multi_thread = 1; // Disable multi-threading for large files to avoid random IO

            (transfers, checkers, multi_thread)
        } else {
            println!("📊 Small files detected ({}% large, avg {:.1}MB) - using optimized transfer settings",
                (large_file_ratio * 100.0) as u32,
                avg_file_size as f64 / 1024.0 / 1024.0
            );

            // Optimized settings for small files
            let transfers = cpu_count;
            let checkers = transfers;
            let multi_thread = 4u32.min(cpu_count); // Allow some multi-threading for small files

            (transfers, checkers, multi_thread)
        }
    }

    pub async fn copy_with_progress(
        &self,
        source: &Path,
        destination: &str,
        dest_config: &Destination,
    ) -> Result<CopyResult> {
        let mut args = vec![
            "copy".to_string(),
            source.to_string_lossy().to_string(),
            format!("{}:{}", dest_config.rclone_remote, destination),
            "--progress".to_string(),
            "--stats=1s".to_string(),
            "--copy-links".to_string(),
        ];

        // Analyze source files to determine optimal transfer settings
        let cpu_count = num_cpus::get() as u32;
        let (optimal_transfers, optimal_checkers, optimal_multi_thread) =
            match Self::analyze_file_sizes(source) {
                Ok((total_files, total_size, large_files)) => Self::calculate_optimal_transfers(
                    total_files,
                    total_size,
                    large_files,
                    cpu_count,
                ),
                Err(e) => {
                    println!(
                        "⚠️  Could not analyze file sizes ({}), using default settings",
                        e
                    );
                    (cpu_count, cpu_count, cpu_count)
                }
            };

        // Add global configuration options
        if let Some(global_config) = &self.global_config {
            if let Some(bandwidth_limit) = &global_config.bandwidth_limit {
                if !bandwidth_limit.is_empty() {
                    args.push("--bwlimit".to_string());
                    args.push(bandwidth_limit.clone());
                }
            }

            // Use configured transfers if specified, otherwise use optimal calculated value
            let transfers = global_config.transfers.unwrap_or(optimal_transfers);
            args.push("--transfers".to_string());
            args.push(transfers.to_string());

            args.push("--checkers".to_string());
            args.push(optimal_checkers.to_string());

            args.push("--multi-thread-streams".to_string());
            args.push(optimal_multi_thread.to_string());

            for flag in &global_config.additional_flags {
                args.push(flag.clone());
            }
        } else {
            // Use optimal calculated values
            args.push("--transfers".to_string());
            args.push(optimal_transfers.to_string());
            args.push("--checkers".to_string());
            args.push(optimal_checkers.to_string());
            args.push("--multi-thread-streams".to_string());
            args.push(optimal_multi_thread.to_string());
        }

        // Use PTY to enable proper terminal behavior
        let pty_system = portable_pty::native_pty_system();
        let pty_size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pty_pair = pty_system.openpty(pty_size)?;
        let mut cmd = CommandBuilder::new("rclone");
        cmd.args(&args);

        let mut child = pty_pair.slave.spawn_command(cmd)?;
        drop(pty_pair.slave);

        // Read and display output in real time
        let reader = pty_pair.master.try_clone_reader()?;
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();

        while buf_reader.read_line(&mut line)? > 0 {
            print!("{}", line);
            std::io::stdout().flush()?;
            line.clear();
        }

        let exit_status = child.wait()?;

        Ok(CopyResult {
            success: exit_status.success(),
            files_transferred: 1,
            bytes_transferred: 0,
            errors: if exit_status.success() {
                Vec::new()
            } else {
                vec![format!(
                    "rclone copy failed with exit code {}",
                    exit_status.exit_code()
                )]
            },
        })
    }

    pub async fn check_integrity(
        &self,
        source: &Path,
        destination: &str,
        dest_config: &Destination,
    ) -> Result<bool> {
        let mut cmd = Command::new("rclone");
        cmd.arg("check")
            .arg(source)
            .arg(format!("{}:{}", dest_config.rclone_remote, destination))
            .arg("--one-way")
            .arg("--copy-links");

        // Add global flags if available
        if let Some(global_config) = &self.global_config {
            for flag in &global_config.additional_flags {
                cmd.arg(flag);
            }
        }

        let status = cmd.status().await?;
        Ok(status.success())
    }

    pub async fn delete_files(&self, remote_path: &str, dest_config: &Destination) -> Result<bool> {
        let mut cmd = Command::new("rclone");
        cmd.arg("delete")
            .arg(format!("{}:{}", dest_config.rclone_remote, remote_path));

        let output = cmd.output().await?;
        Ok(output.status.success())
    }

    pub async fn create_directory(
        &self,
        remote_path: &str,
        dest_config: &Destination,
    ) -> Result<bool> {
        let mut cmd = Command::new("rclone");
        cmd.arg("mkdir")
            .arg(format!("{}:{}", dest_config.rclone_remote, remote_path));

        let output = cmd.output().await?;
        Ok(output.status.success())
    }

    pub async fn list_files(
        &self,
        remote_path: &str,
        dest_config: &Destination,
    ) -> Result<Vec<String>> {
        let mut cmd = Command::new("rclone");
        cmd.arg("ls")
            .arg(format!("{}:{}", dest_config.rclone_remote, remote_path));

        let output = cmd.output().await?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let files = String::from_utf8(output.stdout)?
            .lines()
            .map(|line| {
                // rclone ls format: "    123456 filename.ext"
                line.split_whitespace()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|name| !name.is_empty())
            .collect();

        Ok(files)
    }

    pub async fn get_size(&self, remote_path: &str, dest_config: &Destination) -> Result<u64> {
        let mut cmd = Command::new("rclone");
        cmd.arg("size")
            .arg(format!("{}:{}", dest_config.rclone_remote, remote_path))
            .arg("--json");

        let output = cmd.output().await?;

        if !output.status.success() {
            return Ok(0);
        }

        // Parse JSON output to get size
        let json_str = String::from_utf8(output.stdout)?;
        // Simplified JSON parsing - in practice you'd want to use serde_json
        if let Some(size_start) = json_str.find("\"bytes\":") {
            let size_part = &json_str[size_start + 8..];
            if let Some(size_end) = size_part.find(',').or_else(|| size_part.find('}')) {
                if let Ok(size) = size_part[..size_end].trim().parse::<u64>() {
                    return Ok(size);
                }
            }
        }

        Ok(0)
    }

    pub async fn dry_run_copy(
        &self,
        source: &Path,
        destination: &str,
        dest_config: &Destination,
    ) -> Result<Vec<String>> {
        let mut cmd = Command::new("rclone");
        cmd.arg("copy")
            .arg(source)
            .arg(format!("{}:{}", dest_config.rclone_remote, destination))
            .arg("--dry-run")
            .arg("-v");

        let output = cmd.output().await?;

        let files = String::from_utf8(output.stderr)?
            .lines()
            .filter(|line| line.contains("copy:"))
            .map(|line| line.to_string())
            .collect();

        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_calculate_optimal_transfers_large_files() {
        let cpu_count = 8;

        // Test scenario: mostly large files
        let total_files = 10;
        let total_size = 2_000_000_000; // 2GB total
        let large_files = 8; // 80% are large files

        let (transfers, checkers, multi_thread) = RcloneWrapper::calculate_optimal_transfers(
            total_files,
            total_size,
            large_files,
            cpu_count,
        );

        // Should use conservative settings for large files
        assert_eq!(transfers, 4); // Limited to 4 for large files
        assert_eq!(checkers, 8); // 2x transfers for large files
        assert_eq!(multi_thread, 1); // Disable multi-threading for large files
    }

    #[test]
    fn test_calculate_optimal_transfers_small_files() {
        let cpu_count = 8;

        // Test scenario: mostly small files
        let total_files = 100;
        let total_size = 500_000_000; // 500MB total (avg 5MB per file)
        let large_files = 2; // Only 2% are large files

        let (transfers, checkers, multi_thread) = RcloneWrapper::calculate_optimal_transfers(
            total_files,
            total_size,
            large_files,
            cpu_count,
        );

        // Should use optimized settings for small files
        assert_eq!(transfers, 8); // Use full CPU count
        assert_eq!(checkers, 8); // Same as transfers for small files
        assert_eq!(multi_thread, 4); // Allow multi-threading but capped at 4
    }

    #[test]
    fn test_calculate_optimal_transfers_high_average_size() {
        let cpu_count = 8;

        // Test scenario: few files but very large average size
        let total_files = 5;
        let total_size = 1_000_000_000; // 1GB total (avg 200MB per file)
        let large_files = 3; // 60% are large (>100MB)

        let (transfers, checkers, multi_thread) = RcloneWrapper::calculate_optimal_transfers(
            total_files,
            total_size,
            large_files,
            cpu_count,
        );

        // Should use conservative settings due to high average file size
        assert_eq!(transfers, 4); // Limited to 4 for large files
        assert_eq!(checkers, 8); // 2x transfers for large files
        assert_eq!(multi_thread, 1); // Disable multi-threading for large files
    }

    #[test]
    fn test_analyze_file_sizes() -> Result<()> {
        let temp_dir = TempDir::new()?;

        // Create test files with different sizes
        let small_file = temp_dir.path().join("small.txt");
        let mut file = File::create(&small_file)?;
        file.write_all(&vec![0u8; 1024])?; // 1KB file

        let large_file = temp_dir.path().join("large.txt");
        let mut file = File::create(&large_file)?;
        file.write_all(&vec![0u8; 150 * 1024 * 1024])?; // 150MB file

        let (total_files, total_size, large_files) =
            RcloneWrapper::analyze_file_sizes(temp_dir.path())?;

        assert_eq!(total_files, 2);
        assert_eq!(total_size, 1024 + 150 * 1024 * 1024);
        assert_eq!(large_files, 1); // Only the 150MB file is considered large

        Ok(())
    }

    #[test]
    fn test_analyze_file_sizes_nested_directories() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let nested_dir = temp_dir.path().join("nested");
        std::fs::create_dir(&nested_dir)?;

        // Create files in nested directory
        let file1 = nested_dir.join("file1.txt");
        let mut f = File::create(&file1)?;
        f.write_all(&vec![0u8; 50 * 1024 * 1024])?; // 50MB file (not large)

        let file2 = nested_dir.join("file2.txt");
        let mut f = File::create(&file2)?;
        f.write_all(&vec![0u8; 200 * 1024 * 1024])?; // 200MB file (large)

        let (total_files, total_size, large_files) =
            RcloneWrapper::analyze_file_sizes(temp_dir.path())?;

        assert_eq!(total_files, 2);
        assert_eq!(total_size, 50 * 1024 * 1024 + 200 * 1024 * 1024);
        assert_eq!(large_files, 1); // Only the 200MB file is considered large

        Ok(())
    }
}
