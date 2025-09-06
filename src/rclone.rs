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
    
    pub async fn copy_with_progress(
        &self,
        source: &Path,
        destination: &str,
        dest_config: &Destination,
    ) -> Result<CopyResult>
    {
        let mut args = vec![
            "copy".to_string(),
            source.to_string_lossy().to_string(),
            format!("{}:{}", dest_config.rclone_remote, destination),
            "--progress".to_string(),
            "--stats=1s".to_string(),
            "--copy-links".to_string(),
        ];
        
        // Add global configuration options
        if let Some(global_config) = &self.global_config {
            if let Some(bandwidth_limit) = &global_config.bandwidth_limit {
                args.push("--bwlimit".to_string());
                args.push(bandwidth_limit.clone());
            }
            
            let transfers = global_config.transfers.unwrap_or_else(|| num_cpus::get() as u32);
            args.push("--transfers".to_string());
            args.push(transfers.to_string());
            
            args.push("--checkers".to_string());
            args.push(transfers.to_string());
            
            args.push("--multi-thread-streams".to_string());
            args.push(transfers.to_string());
            
            for flag in &global_config.additional_flags {
                args.push(flag.clone());
            }
        } else {
            // Default to CPU count if no global config
            let cpu_count = num_cpus::get() as u32;
            args.push("--transfers".to_string());
            args.push(cpu_count.to_string());
            args.push("--checkers".to_string());
            args.push(cpu_count.to_string());
            args.push("--multi-thread-streams".to_string());
            args.push(cpu_count.to_string());
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
            errors: Vec::new(),
        })
    }
    
    
    pub async fn check_integrity(&self, source: &Path, destination: &str, dest_config: &Destination) -> Result<bool> {
        let mut cmd = Command::new("rclone");
        cmd.arg("check")
            .arg(source)
            .arg(format!("{}:{}", dest_config.rclone_remote, destination))
            .arg("--one-way");
        
        // Add global flags if available
        if let Some(global_config) = &self.global_config {
            for flag in &global_config.additional_flags {
                cmd.arg(flag);
            }
        }
        
        let output = cmd.output().await?;
        Ok(output.status.success())
    }
    
    pub async fn delete_files(&self, remote_path: &str, dest_config: &Destination) -> Result<bool> {
        let mut cmd = Command::new("rclone");
        cmd.arg("delete")
            .arg(format!("{}:{}", dest_config.rclone_remote, remote_path));
        
        let output = cmd.output().await?;
        Ok(output.status.success())
    }
    
    pub async fn create_directory(&self, remote_path: &str, dest_config: &Destination) -> Result<bool> {
        let mut cmd = Command::new("rclone");
        cmd.arg("mkdir")
            .arg(format!("{}:{}", dest_config.rclone_remote, remote_path));
        
        let output = cmd.output().await?;
        Ok(output.status.success())
    }
    
    pub async fn list_files(&self, remote_path: &str, dest_config: &Destination) -> Result<Vec<String>> {
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
    
    pub async fn dry_run_copy(&self, source: &Path, destination: &str, dest_config: &Destination) -> Result<Vec<String>> {
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