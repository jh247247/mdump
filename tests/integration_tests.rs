use mdump::{FileProcessor, HostConfig, MediaDetector, TemplateProcessor};
use std::collections::HashMap;
use tempfile::TempDir;

/// Integration test for the complete backup workflow
#[tokio::test]
async fn test_end_to_end_backup_workflow() -> mdump::Result<()> {
    // Create temporary directories for source and destination
    let temp_dir = TempDir::new()?;
    let source_mount = temp_dir.path().join("source_media");
    let destination_path = temp_dir.path().join("backup_dest");

    std::fs::create_dir_all(&source_mount)?;
    std::fs::create_dir_all(&destination_path)?;

    // Create test media structure
    let dcim_dir = source_mount.join("DCIM");
    std::fs::create_dir_all(&dcim_dir)?;

    // Create test files
    std::fs::write(dcim_dir.join("IMG001.jpg"), "fake jpg content 1")?;
    std::fs::write(dcim_dir.join("IMG002.jpg"), "fake jpg content 2")?;
    std::fs::write(dcim_dir.join("VIDEO001.mp4"), "fake mp4 content")?;
    std::fs::write(dcim_dir.join("backup.bak"), "backup file")?; // Should be excluded

    // Create mdump source config
    let source_config_content = r#"
[source]
name = "Test Camera"
description = "Integration test camera"
paths = ["DCIM/"]
file_filters = ["*.jpg", "*.mp4"]
exclude_patterns = ["*.bak"]
"#;

    std::fs::write(
        source_mount.join("mdump_source.toml"),
        source_config_content,
    )?;

    // Create host configuration
    let mut destinations = HashMap::new();
    destinations.insert(
        "test_dest".to_string(),
        mdump::config::Destination {
            path: destination_path.to_str().unwrap().to_string(),
            rclone_remote: "test_remote".to_string(),
            name_template: "{media_name}_{original_name}".to_string(),
            processing: mdump::config::ProcessingConfig {
                flatten_folders: true,
                directory_structure: "{yyyy}/{mm}".to_string(),
            },
        },
    );

    let host_config = HostConfig {
        destinations,
        remotes: None,
        rclone: None,
        security: Some(mdump::config::SecurityConfig {
            verify_integrity: false, // Skip for integration test
            require_confirmation_before_delete: false,
            hash_collision_detection: None,
        }),
    };

    // Test media detection
    let media_detector = MediaDetector::new();

    // Manually create detected media (since we're not actually mounting)
    let media_config = mdump::config::MediaConfig::load(&source_mount.join("mdump_source.toml"))?;
    let detected_media = mdump::config::DetectedMedia {
        name: media_config.source.name.clone(),
        description: media_config.source.description.clone(),
        mount_path: source_mount.clone(),
        config: media_config,
    };

    // Test file collection
    let files = media_detector.collect_files(&detected_media)?;
    assert_eq!(files.len(), 3); // Should find 2 jpg + 1 mp4, but not .bak

    // Test template processing and file mapping
    let destination = host_config.get_destination("test_dest").unwrap();
    let mut template_processor =
        TemplateProcessor::new(&detected_media, destination, &host_config)?;

    // Simulate file processing (dry run to avoid rclone dependency)
    let file_processor = FileProcessor::new(&host_config, true); // dry_run = true
    let result = file_processor
        .process_media(&detected_media, destination, &host_config)
        .await?;

    // Verify results
    assert_eq!(result.files_processed, 3);
    assert!(result.errors.is_empty());
    assert_eq!(result.skipped_files.len(), 0);

    // Test that file mappings are created correctly
    let files_to_process = file_processor.collect_files_to_process(&detected_media)?;
    assert_eq!(files_to_process.len(), 3);

    let file_mappings = file_processor.create_file_mappings(
        &files_to_process,
        &detected_media,
        destination,
        &mut template_processor,
    )?;

    // Verify all files have proper mappings
    assert_eq!(file_mappings.len(), 3);

    for mapping in &file_mappings {
        // All processed filenames should start with the media name
        assert!(mapping.processed_filename.contains("Test Camera"));

        // Destination paths should be properly formed
        assert!(mapping
            .destination_path
            .starts_with(destination_path.to_str().unwrap()));

        // Source paths should be valid
        assert!(mapping.source_path.exists());
    }

    // Verify file name uniqueness
    let processed_names: Vec<_> = file_mappings
        .iter()
        .map(|m| &m.processed_filename)
        .collect();
    let mut unique_names = processed_names.clone();
    unique_names.sort();
    unique_names.dedup();
    assert_eq!(processed_names.len(), unique_names.len()); // All names should be unique

    Ok(())
}

/// Test configuration loading and validation
#[test]
fn test_configuration_workflow() -> mdump::Result<()> {
    let temp_dir = TempDir::new()?;
    let config_path = temp_dir.path().join("host_config.toml");

    // Create a comprehensive host configuration
    let config_content = r#"
[destinations.primary]
path = "/backup/primary"
rclone_remote = "s3:bucket"
name_template = "{media_name}_{date}_{original_name}"

[destinations.primary.processing]
flatten_folders = false
directory_structure = "{yyyy}/{mm}/{dd}"

[destinations.secondary]
path = "/backup/secondary"
rclone_remote = "gdrive:"
name_template = "{content_hash_short}_{original_name}"

[destinations.secondary.processing]
flatten_folders = true
directory_structure = "archived/{yyyy}"

[remotes.s3]
type = "s3"
provider = "AWS"
access_key_id = "test_key"
secret_access_key = "test_secret"
region = "us-east-1"

[remotes.gdrive]
type = "drive"
client_id = "test_client_id"
client_secret = "test_client_secret"

[rclone]
bandwidth_limit = "100M"
transfers = 8
additional_flags = ["--progress", "--stats", "30s"]

[security]
verify_integrity = true
require_confirmation_before_delete = true

[security.hash_collision_detection]
initial_read_size = 1048576
max_read_size = 16777216
collision_multiplier = 2.0
enable_progressive_hashing = true
"#;

    std::fs::write(&config_path, config_content)?;

    // Load and validate the configuration
    let host_config = HostConfig::load(&config_path)?;

    // Verify destinations
    assert_eq!(host_config.destinations.len(), 2);
    assert!(host_config.get_destination("primary").is_some());
    assert!(host_config.get_destination("secondary").is_some());

    let primary = host_config.get_destination("primary").unwrap();
    assert_eq!(primary.path, "/backup/primary");
    assert_eq!(primary.rclone_remote, "s3:bucket");
    assert!(!primary.processing.flatten_folders);
    assert_eq!(primary.processing.directory_structure, "{yyyy}/{mm}/{dd}");

    let secondary = host_config.get_destination("secondary").unwrap();
    assert_eq!(secondary.path, "/backup/secondary");
    assert_eq!(secondary.rclone_remote, "gdrive:");
    assert!(secondary.processing.flatten_folders);
    assert_eq!(secondary.processing.directory_structure, "archived/{yyyy}");

    // Verify remotes
    assert!(host_config.remotes.is_some());
    let remotes = host_config.remotes.unwrap();
    assert_eq!(remotes.len(), 2);
    assert!(remotes.contains_key("s3"));
    assert!(remotes.contains_key("gdrive"));

    // Verify rclone configuration
    assert!(host_config.rclone.is_some());
    let rclone = host_config.rclone.unwrap();
    assert_eq!(rclone.bandwidth_limit, Some("100M".to_string()));
    assert_eq!(rclone.transfers, Some(8));
    assert_eq!(rclone.additional_flags.len(), 3);

    // Verify security configuration
    assert!(host_config.security.is_some());
    let security = host_config.security.unwrap();
    assert!(security.verify_integrity);
    assert!(security.require_confirmation_before_delete);

    assert!(security.hash_collision_detection.is_some());
    let hash_config = security.hash_collision_detection.unwrap();
    assert!(hash_config.enable_progressive_hashing);
    assert_eq!(hash_config.initial_read_size, Some(1048576));

    Ok(())
}

/// Test template variable resolution
#[test]
fn test_template_variable_workflow() -> mdump::Result<()> {
    let temp_dir = TempDir::new()?;

    // Create test media structure
    let media_path = temp_dir.path().join("test_media");
    std::fs::create_dir_all(&media_path)?;

    let test_file = media_path.join("test_photo.jpg");
    std::fs::write(&test_file, "test image content")?;

    // Create media configuration
    let media_config = mdump::config::MediaConfig {
        source: mdump::config::MediaSource {
            name: "Test Camera".to_string(),
            description: "My test camera".to_string(),
            paths: vec![".".to_string()],
            file_filters: vec!["*.jpg".to_string()],
            exclude_patterns: vec![],
            deletion: None,
            validation: None,
            pre_processing: None,
            post_processing: None,
        },
    };

    let detected_media = mdump::config::DetectedMedia {
        name: media_config.source.name.clone(),
        description: media_config.source.description.clone(),
        mount_path: media_path,
        config: media_config,
    };

    // Create destination with various template variables
    let destination = mdump::config::Destination {
        path: "/backup/test".to_string(),
        rclone_remote: "test:".to_string(),
        name_template: "{media_name}_{yyyy}-{mm}-{dd}_{original_name}".to_string(),
        processing: mdump::config::ProcessingConfig {
            flatten_folders: false,
            directory_structure: "{media_name}/{yyyy}/{mm}".to_string(),
        },
    };

    let host_config = HostConfig {
        destinations: HashMap::new(),
        remotes: None,
        rclone: None,
        security: Some(mdump::config::SecurityConfig::default()),
    };

    // Test template processor
    let mut template_processor =
        TemplateProcessor::new(&detected_media, &destination, &host_config)?;

    // Test global variables
    let media_name_result = template_processor.process_template("{media_name}")?;
    assert_eq!(media_name_result, "Test Camera");

    // Test date variables (should have current date)
    let date_result = template_processor.process_template("{yyyy}")?;
    assert_eq!(date_result.len(), 4); // Should be a 4-digit year

    // Set file-specific variables
    template_processor.set_file_variables(&test_file, "test_photo.jpg")?;

    // Test file-specific variables
    let original_name = template_processor.process_template("{original_name}")?;
    assert_eq!(original_name, "test_photo.jpg");

    let name_without_ext = template_processor.process_template("{name}")?;
    assert_eq!(name_without_ext, "test_photo");

    let extension = template_processor.process_template("{ext}")?;
    assert_eq!(extension, "jpg");

    // Test complex template
    let complex_result =
        template_processor.process_filename_template(&destination.name_template)?;
    assert!(complex_result.contains("Test Camera"));
    assert!(complex_result.contains("test_photo.jpg"));

    // Test directory template
    let dir_result = template_processor
        .process_directory_template(&destination.processing.directory_structure)?;
    assert!(dir_result.to_string_lossy().contains("Test Camera"));

    Ok(())
}

/// Integration test for pre-processing config parsing
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

/// End-to-end test for dji-joiner pre-processing (requires external binaries)
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
            copy_input: false,
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
    assert_eq!(result.files_to_backup.len(), 1,
               "Expected 1 joined file, got: {:?}", result.files_to_backup);
    assert_eq!(result.original_source_files.len(), 2);

    let output_name = result.files_to_backup[0].file_name().unwrap().to_string_lossy();
    assert!(output_name.starts_with("DJI_Recording_"),
            "Unexpected output name: {}", output_name);
}
