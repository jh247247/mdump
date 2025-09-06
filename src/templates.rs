use crate::config::{DetectedMedia, Destination, HostConfig, HashCollisionConfig};
use crate::Result;
use chrono::{DateTime, Local};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub struct TemplateProcessor {
    variables: HashMap<String, String>,
    file_variables: HashMap<String, String>,
    hash_cache: Arc<Mutex<HashCache>>,
    collision_config: HashCollisionConfig,
}

#[derive(Debug)]
struct HashCache {
    hashes: HashMap<String, HashEntry>,
}

#[derive(Debug, Clone)]
struct HashEntry {
    file_path: PathBuf,
    #[allow(dead_code)]
    read_size: u64,
    #[allow(dead_code)]
    hash: String,
}

impl HashCache {
    fn new() -> Self {
        Self {
            hashes: HashMap::new(),
        }
    }
    
    fn check_collision(&self, hash: &str, file_path: &Path) -> Option<&HashEntry> {
        if let Some(existing) = self.hashes.get(hash) {
            if existing.file_path != file_path {
                return Some(existing);
            }
        }
        None
    }
    
    fn insert(&mut self, hash: String, file_path: PathBuf, read_size: u64) {
        self.hashes.insert(hash.clone(), HashEntry {
            file_path,
            read_size,
            hash,
        });
    }
}

impl TemplateProcessor {
    fn read_first_mb(file_path: &Path) -> Result<Vec<u8>> {
        Self::read_file_bytes(file_path, 1024 * 1024)
    }
    
    fn read_file_bytes(file_path: &Path, size: u64) -> Result<Vec<u8>> {
        let mut file = File::open(file_path)?;
        let mut buffer = vec![0u8; size as usize];
        let bytes_read = file.read(&mut buffer)?;
        buffer.truncate(bytes_read);
        Ok(buffer)
    }
    
    pub fn new(media: &DetectedMedia, _destination: &Destination, host_config: &HostConfig) -> Result<Self> {
        let now: DateTime<Local> = Local::now();
        let hostname = gethostname::gethostname()
            .to_string_lossy()
            .to_string();
        
        let mut variables = HashMap::new();
        
        // Media-specific variables
        variables.insert("media_name".to_string(), media.name.clone());
        variables.insert("description".to_string(), media.description.clone());
        
        // Time variables
        variables.insert("date".to_string(), now.format("%Y-%m-%d").to_string());
        variables.insert("yyyy".to_string(), now.format("%Y").to_string());
        variables.insert("mm".to_string(), now.format("%m").to_string());
        variables.insert("dd".to_string(), now.format("%d").to_string());
        variables.insert("time".to_string(), now.format("%H-%M-%S").to_string());
        
        // System variables
        variables.insert("uuid".to_string(), Uuid::new_v4().to_string());
        variables.insert("hostname".to_string(), hostname);
        
        // Get collision detection configuration
        let collision_config = host_config
            .security
            .as_ref()
            .and_then(|s| s.hash_collision_detection.as_ref())
            .cloned()
            .unwrap_or_default();
        
        let hash_cache = Arc::new(Mutex::new(HashCache::new()));
        
        // Compute content hash from largest file for uniqueness
        let content_hash = Self::compute_content_hash_with_collision_detection(media, &hash_cache, &collision_config)?;
        variables.insert("content_hash".to_string(), content_hash.clone());
        variables.insert("content_hash_short".to_string(), content_hash[..8].to_string());
        
        Ok(Self {
            variables,
            file_variables: HashMap::new(),
            hash_cache,
            collision_config,
        })
    }
    
    fn compute_content_hash_with_collision_detection(
        media: &DetectedMedia,
        hash_cache: &Arc<Mutex<HashCache>>,
        collision_config: &HashCollisionConfig,
    ) -> Result<String> {
        let mut largest_file: Option<(PathBuf, u64)> = None;
        
        // Find the largest file in the media paths
        for path_str in &media.config.source.paths {
            let full_path = media.mount_path.join(path_str);
            if full_path.exists() {
                Self::find_largest_file_recursive(&full_path, &mut largest_file)?;
            }
        }
        
        match largest_file {
            Some((file_path, _)) => {
                Self::compute_progressive_hash(&file_path, hash_cache, collision_config)
            }
            None => {
                // Fallback: hash the media name and mount path
                let fallback_content = format!("{}{}", media.name, media.mount_path.display());
                let hash = Sha256::digest(fallback_content.as_bytes());
                Ok(format!("{:x}", hash))
            }
        }
    }
    
    fn compute_progressive_hash(
        file_path: &Path,
        hash_cache: &Arc<Mutex<HashCache>>,
        collision_config: &HashCollisionConfig,
    ) -> Result<String> {
        if !collision_config.enable_progressive_hashing {
            // Use legacy behavior
            let content = Self::read_first_mb(file_path)?;
            let hash = Sha256::digest(&content);
            return Ok(format!("{:x}", hash));
        }
        
        let initial_size = collision_config.initial_read_size.unwrap_or(1024 * 1024);
        let max_size = collision_config.max_read_size.unwrap_or(16 * 1024 * 1024);
        let multiplier = collision_config.collision_multiplier.unwrap_or(2.0);
        
        let mut current_size = initial_size;
        
        loop {
            // Read the current amount of data
            let content = Self::read_file_bytes(file_path, current_size)?;
            let hash = Sha256::digest(&content);
            let hash_str = format!("{:x}", hash);
            
            // Check for collision
            let collision = {
                let cache = hash_cache.lock().unwrap();
                cache.check_collision(&hash_str, file_path).cloned()
            };
            
            if let Some(existing_entry) = collision {
                println!("⚠️  Hash collision detected!");
                println!("   Current file: {}", file_path.display());
                println!("   Existing file: {}", existing_entry.file_path.display());
                println!("   Hash: {}...", &hash_str[..16]);
                
                // If we've reached the maximum read size, we'll have to accept the collision
                if current_size >= max_size {
                    println!("   Maximum read size reached, accepting collision");
                    let mut cache = hash_cache.lock().unwrap();
                    cache.insert(hash_str.clone(), file_path.to_path_buf(), current_size);
                    return Ok(hash_str);
                }
                
                // Increase the read size and try again
                current_size = (current_size as f64 * multiplier) as u64;
                current_size = current_size.min(max_size);
                
                println!("   Increasing read size to {} bytes and retrying", current_size);
                continue;
            }
            
            // No collision, store the hash and return
            {
                let mut cache = hash_cache.lock().unwrap();
                cache.insert(hash_str.clone(), file_path.to_path_buf(), current_size);
            }
            
            if current_size > initial_size {
                println!("✅ Resolved hash collision using {} bytes", current_size);
            }
            
            return Ok(hash_str);
        }
    }
    
    fn find_largest_file_recursive(
        path: &Path,
        largest: &mut Option<(PathBuf, u64)>,
    ) -> Result<()> {
        if path.is_file() {
            let size = std::fs::metadata(path)?.len();
            if largest.as_ref().map_or(true, |(_, current_size)| size > *current_size) {
                *largest = Some((path.to_path_buf(), size));
            }
        } else if path.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                Self::find_largest_file_recursive(&entry.path(), largest)?;
            }
        }
        Ok(())
    }
    
    pub fn set_file_variables(&mut self, file_path: &Path, original_relative_path: &str) -> Result<()> {
        self.file_variables.clear();
        
        // File-specific variables
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        
        let name_without_ext = file_path
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        
        let extension = file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();
        
        self.file_variables.insert("original_name".to_string(), file_name);
        self.file_variables.insert("name".to_string(), name_without_ext);
        self.file_variables.insert("ext".to_string(), extension);
        
        // Split the relative path into directory and full path
        let original_dir = Path::new(original_relative_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| String::new());
        
        self.file_variables.insert("original_path".to_string(), original_relative_path.to_string());
        self.file_variables.insert("original_dir".to_string(), original_dir);
        
        // Compute file-specific content hash
        let file_hash = self.compute_file_hash(file_path)?;
        self.file_variables.insert("content_hash".to_string(), file_hash.clone());
        self.file_variables.insert("content_hash_short".to_string(), file_hash[..8].to_string());
        
        Ok(())
    }
    
    fn compute_file_hash(&self, file_path: &Path) -> Result<String> {
        Self::compute_progressive_hash(file_path, &self.hash_cache, &self.collision_config)
    }
    
    pub fn process_template(&self, template: &str) -> Result<String> {
        let mut result = template.to_string();
        
        // Replace file-specific variables first (they take precedence over global ones)
        for (key, value) in &self.file_variables {
            let placeholder = format!("{{{}}}", key);
            result = result.replace(&placeholder, value);
        }
        
        // Replace global variables for any remaining placeholders
        for (key, value) in &self.variables {
            let placeholder = format!("{{{}}}", key);
            result = result.replace(&placeholder, value);
        }
        
        // Clean up any remaining variables and sanitize for filesystem
        result = self.sanitize_path(&result)?;
        
        Ok(result)
    }
    
    pub fn process_directory_template(&self, template: &str) -> Result<PathBuf> {
        let processed = self.process_template(template)?;
        Ok(PathBuf::from(processed))
    }
    
    pub fn process_filename_template(&self, template: &str) -> Result<String> {
        let processed = self.process_template(template)?;
        
        // Ensure we have a valid filename
        if processed.is_empty() {
            return Ok("unnamed_file".to_string());
        }
        
        Ok(processed)
    }
    
    fn sanitize_path(&self, path: &str) -> Result<String> {
        // Replace or remove characters that are problematic in file paths
        let sanitized = path
            .chars()
            .map(|c| match c {
                // Replace problematic characters with underscores
                '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
                // Keep forward slashes for directory separation
                '/' => '/',
                // Replace backslashes with forward slashes for consistency
                '\\' => '/',
                // Keep other characters
                _ => c,
            })
            .collect::<String>();
        
        // Remove any double slashes
        let sanitized = regex::Regex::new(r"/+")?.replace_all(&sanitized, "/");
        
        // Remove leading/trailing slashes and whitespace
        Ok(sanitized.trim_matches('/').trim().to_string())
    }
    
    pub fn get_variable(&self, key: &str) -> Option<&String> {
        self.file_variables.get(key).or_else(|| self.variables.get(key))
    }
    
    pub fn list_available_variables(&self) -> Vec<String> {
        let mut vars: Vec<String> = self.variables.keys()
            .chain(self.file_variables.keys())
            .cloned()
            .collect();
        vars.sort();
        vars
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MediaConfig, MediaSource};
    
    fn create_test_media() -> DetectedMedia {
        let config = MediaConfig {
            source: MediaSource {
                name: "Test Media".to_string(),
                description: "Test Description".to_string(),
                paths: vec!["test/".to_string()],
                file_filters: vec!["*.jpg".to_string()],
                exclude_patterns: vec![],
                deletion: None,
            },
        };
        
        DetectedMedia {
            name: "Test Media".to_string(),
            description: "Test Description".to_string(),
            mount_path: PathBuf::from("/tmp/test"),
            config,
        }
    }
    
    fn create_test_destination() -> Destination {
        Destination {
            path: "test_path".to_string(),
            rclone_remote: "test_remote".to_string(),
            name_template: "{media_name}_{date}".to_string(),
            processing: crate::config::ProcessingConfig {
                flatten_folders: false,
                directory_structure: "{yyyy}/{mm}".to_string(),
            },
        }
    }
    
    fn create_test_host_config() -> HostConfig {
        HostConfig {
            destinations: HashMap::new(),
            remotes: None,
            rclone: None,
            security: Some(crate::config::SecurityConfig::default()),
        }
    }
    
    #[test]
    fn test_template_processing() -> Result<()> {
        let media = create_test_media();
        let destination = create_test_destination();
        
        let host_config = create_test_host_config();
        let processor = TemplateProcessor::new(&media, &destination, &host_config)?;
        let result = processor.process_template("{media_name}_test")?;
        
        assert!(result.contains("Test Media_test"));
        Ok(())
    }
    
    #[test]
    fn test_path_sanitization() -> Result<()> {
        let media = create_test_media();
        let destination = create_test_destination();
        
        let host_config = create_test_host_config();
        let processor = TemplateProcessor::new(&media, &destination, &host_config)?;
        let result = processor.sanitize_path("test<>:path|with?bad*chars")?;
        
        // All problematic characters should be replaced with underscores
        assert_eq!(result, "test___path_with_bad_chars");
        Ok(())
    }
}