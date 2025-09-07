use crate::config::{DetectedMedia, MediaConfig};
use crate::Result;
use std::path::{Path, PathBuf};
use tokio::fs;
use walkdir::WalkDir;

pub struct MediaDetector;

impl Default for MediaDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaDetector {
    pub fn new() -> Self {
        Self
    }

    pub async fn scan_for_media(&self) -> Result<Vec<DetectedMedia>> {
        let mut detected_media = Vec::new();
        let mount_points = self.get_mount_points().await?;

        for mount_point in mount_points {
            if let Some(media) = self.check_mount_point(&mount_point).await? {
                detected_media.push(media);
            }
        }

        Ok(detected_media)
    }

    async fn get_mount_points(&self) -> Result<Vec<PathBuf>> {
        let mut mount_points = Vec::new();

        // On macOS, removable media is typically mounted under /Volumes
        #[cfg(target_os = "macos")]
        {
            let volumes_dir = Path::new("/Volumes");
            if volumes_dir.exists() {
                let mut entries = fs::read_dir(volumes_dir).await?;
                while let Some(entry) = entries.next_entry().await? {
                    let path = entry.path();
                    if path.is_dir() && self.is_removable_media(&path).await? {
                        mount_points.push(path);
                    }
                }
            }
        }

        // On Linux, check /media and /mnt
        #[cfg(target_os = "linux")]
        {
            for base_dir in ["/media", "/mnt"] {
                let base_path = Path::new(base_dir);
                if base_path.exists() {
                    for entry in WalkDir::new(base_path).max_depth(2) {
                        let entry = entry?;
                        let path = entry.path();
                        if path.is_dir() && self.is_removable_media(path).await? {
                            mount_points.push(path.to_path_buf());
                        }
                    }
                }
            }
        }

        // On Windows, check drive letters
        #[cfg(target_os = "windows")]
        {
            for letter in 'A'..='Z' {
                let drive_path = PathBuf::from(format!("{}:\\", letter));
                if drive_path.exists() && self.is_removable_media(&drive_path).await? {
                    mount_points.push(drive_path);
                }
            }
        }

        Ok(mount_points)
    }

    async fn is_removable_media(&self, path: &Path) -> Result<bool> {
        // Basic heuristics to determine if this is removable media
        // This is a simplified implementation - in practice, you might want to use
        // platform-specific APIs to properly detect removable media

        // Skip system directories that are clearly not removable media
        if let Some("Macintosh HD" | "System" | "Library" | "Applications") =
            path.file_name().and_then(|n| n.to_str())
        {
            return Ok(false);
        }

        // Check if the path is writable (basic test for mounted removable media)
        match fs::metadata(path).await {
            Ok(metadata) => Ok(!metadata.permissions().readonly()),
            Err(_) => Ok(false),
        }
    }

    async fn check_mount_point(&self, mount_point: &Path) -> Result<Option<DetectedMedia>> {
        let config_path = mount_point.join("mdump_source.toml");

        if !config_path.exists() {
            return Ok(None);
        }

        match MediaConfig::load(&config_path) {
            Ok(config) => {
                let detected = DetectedMedia {
                    name: config.source.name.clone(),
                    description: config.source.description.clone(),
                    mount_path: mount_point.to_path_buf(),
                    config,
                };
                Ok(Some(detected))
            }
            Err(e) => {
                eprintln!(
                    "Warning: Found mdump_source.toml at {} but couldn't parse it: {}",
                    config_path.display(),
                    e
                );
                Ok(None)
            }
        }
    }

    pub fn validate_media_paths(&self, media: &DetectedMedia) -> Result<Vec<PathBuf>> {
        let mut valid_paths = Vec::new();

        for path_str in &media.config.source.paths {
            let full_path = media.mount_path.join(path_str);
            if full_path.exists() {
                valid_paths.push(full_path);
            } else {
                println!(
                    "Warning: Path '{}' not found on media '{}'",
                    path_str, media.name
                );
            }
        }

        Ok(valid_paths)
    }

    pub fn collect_files(&self, media: &DetectedMedia) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        let valid_paths = self.validate_media_paths(media)?;

        for path in valid_paths {
            if path.is_file() {
                if self.matches_filters(&path, media)? {
                    files.push(path);
                }
            } else if path.is_dir() {
                for entry in WalkDir::new(&path) {
                    let entry = entry?;
                    let file_path = entry.path();

                    if file_path.is_file() && self.matches_filters(file_path, media)? {
                        files.push(file_path.to_path_buf());
                    }
                }
            }
        }

        Ok(files)
    }

    fn matches_filters(&self, file_path: &Path, media: &DetectedMedia) -> Result<bool> {
        let file_name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        // Check exclude patterns first
        for pattern in &media.config.source.exclude_patterns {
            if self.matches_pattern(file_name, pattern)? {
                return Ok(false);
            }
        }

        // If no file filters are specified, include all files
        if media.config.source.file_filters.is_empty() {
            return Ok(true);
        }

        // Check if file matches any of the include patterns
        for pattern in &media.config.source.file_filters {
            if self.matches_pattern(file_name, pattern)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn matches_pattern(&self, file_name: &str, pattern: &str) -> Result<bool> {
        // Simple glob pattern matching - convert to regex
        let regex_pattern = pattern
            .replace(".", r"\.")
            .replace("*", ".*")
            .replace("?", ".");

        let regex = regex::Regex::new(&format!("^{}$", regex_pattern))?;
        Ok(regex.is_match(file_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MediaConfig, MediaSource};
    use tempfile::TempDir;

    fn create_test_media_config() -> MediaConfig {
        MediaConfig {
            source: MediaSource {
                name: "Test Camera".to_string(),
                description: "Test camera media".to_string(),
                paths: vec!["DCIM/".to_string(), "Pictures/".to_string()],
                file_filters: vec!["*.jpg".to_string(), "*.mp4".to_string()],
                exclude_patterns: vec!["*.bak".to_string(), "*.tmp".to_string()],
                deletion: None,
                validation: None,
            },
        }
    }

    #[test]
    fn test_pattern_matching() -> Result<()> {
        let detector = MediaDetector::new();

        // Test basic wildcard matching
        assert!(detector.matches_pattern("photo.jpg", "*.jpg")?);
        assert!(detector.matches_pattern("video.mp4", "*.mp4")?);
        assert!(!detector.matches_pattern("document.txt", "*.jpg")?);

        // Test question mark wildcard
        assert!(detector.matches_pattern("IMG1.jpg", "IMG?.jpg")?);
        assert!(!detector.matches_pattern("IMG12.jpg", "IMG?.jpg")?);

        // Test exact match
        assert!(detector.matches_pattern("exact_name.jpg", "exact_name.jpg")?);
        assert!(!detector.matches_pattern("different_name.jpg", "exact_name.jpg")?);

        Ok(())
    }

    #[test]
    fn test_file_filtering() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let detector = MediaDetector::new();
        let config = create_test_media_config();

        let media = DetectedMedia {
            name: config.source.name.clone(),
            description: config.source.description.clone(),
            mount_path: temp_dir.path().to_path_buf(),
            config,
        };

        // Test included files
        assert!(detector.matches_filters(&temp_dir.path().join("photo.jpg"), &media)?);
        assert!(detector.matches_filters(&temp_dir.path().join("video.mp4"), &media)?);

        // Test excluded files
        assert!(!detector.matches_filters(&temp_dir.path().join("backup.bak"), &media)?);
        assert!(!detector.matches_filters(&temp_dir.path().join("temp.tmp"), &media)?);

        // Test files not in filter
        assert!(!detector.matches_filters(&temp_dir.path().join("document.txt"), &media)?);

        Ok(())
    }

    #[test]
    fn test_config_loading_from_mount_point() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let mount_path = temp_dir.path();
        let detector = MediaDetector::new();

        // Create a test config file
        let config_content = r#"
[source]
name = "Test SD Card"
description = "My camera's SD card"
paths = ["DCIM/", "Pictures/"]
file_filters = ["*.jpg", "*.raw"]
exclude_patterns = ["*.bak"]
"#;

        let config_path = mount_path.join("mdump_source.toml");
        std::fs::write(&config_path, config_content)?;

        // Create a basic runtime for async test
        let rt = tokio::runtime::Runtime::new()?;
        let result = rt.block_on(async { detector.check_mount_point(mount_path).await })?;

        assert!(result.is_some());
        let detected = result.unwrap();
        assert_eq!(detected.name, "Test SD Card");
        assert_eq!(detected.description, "My camera's SD card");
        assert_eq!(detected.mount_path, mount_path);
        assert_eq!(detected.config.source.paths, vec!["DCIM/", "Pictures/"]);

        Ok(())
    }

    #[test]
    fn test_path_validation() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let detector = MediaDetector::new();
        let config = create_test_media_config();

        // Create some test directories
        std::fs::create_dir_all(temp_dir.path().join("DCIM"))?;
        std::fs::create_dir_all(temp_dir.path().join("Pictures"))?;

        let media = DetectedMedia {
            name: config.source.name.clone(),
            description: config.source.description.clone(),
            mount_path: temp_dir.path().to_path_buf(),
            config,
        };

        let valid_paths = detector.validate_media_paths(&media)?;

        assert_eq!(valid_paths.len(), 2);
        assert!(valid_paths.iter().any(|p| p.ends_with("DCIM")));
        assert!(valid_paths.iter().any(|p| p.ends_with("Pictures")));

        Ok(())
    }

    #[test]
    fn test_file_collection() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let detector = MediaDetector::new();

        // Create directory structure with test files
        let dcim_dir = temp_dir.path().join("DCIM");
        std::fs::create_dir_all(&dcim_dir)?;

        // Create test files
        std::fs::write(dcim_dir.join("IMG001.jpg"), "fake jpg content")?;
        std::fs::write(dcim_dir.join("VID001.mp4"), "fake mp4 content")?;
        std::fs::write(dcim_dir.join("backup.bak"), "backup content")?; // Should be excluded
        std::fs::write(dcim_dir.join("readme.txt"), "text content")?; // Should be excluded

        let config = create_test_media_config();
        let media = DetectedMedia {
            name: config.source.name.clone(),
            description: config.source.description.clone(),
            mount_path: temp_dir.path().to_path_buf(),
            config,
        };

        let files = detector.collect_files(&media)?;

        // Should find 2 files (jpg and mp4), but not bak or txt
        assert_eq!(files.len(), 2);

        let file_names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_str().unwrap().to_string())
            .collect();

        assert!(file_names.contains(&"IMG001.jpg".to_string()));
        assert!(file_names.contains(&"VID001.mp4".to_string()));
        assert!(!file_names.contains(&"backup.bak".to_string()));
        assert!(!file_names.contains(&"readme.txt".to_string()));

        Ok(())
    }

    #[tokio::test]
    async fn test_mount_point_without_config() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let detector = MediaDetector::new();

        // Don't create any config file
        let result = detector.check_mount_point(temp_dir.path()).await?;

        assert!(result.is_none());
        Ok(())
    }
}
