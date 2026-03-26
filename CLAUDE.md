# mdump Development Guide

Media backup tool using rclone for backing up photos/videos from removable media to cloud storage with intelligent organization and deduplication.

## Key Solutions

### File Deletion & Backup Logging
- **Problem**: rclone checksum delays prevented successful backup completion, blocking deletion
- **Solution**: Independent delete subcommand using backup logs (`src/operations.rs`)
- **Files**: `~/Library/Application Support/mdump/logs/{media_name}.json`

### File Date Correction
- **Problem**: Files named with backup date instead of creation date
- **Solution**: Use `metadata.created()` with fallback to modified time in `src/templates.rs`
- **Result**: `20250906_file.mp4` (creation) vs `20250907_file.mp4` (backup)

### rclone Transfer Optimization
- **Problem**: CPU-count transfers caused thrashing with large video files
- **Solution**: File-size-based optimization (`src/rclone.rs`)
  - Large files (>100MB): 4 transfers, single-threaded streams
  - Small files: CPU-count transfers, 4 streams
- **Impact**: 4GB videos use 4 conservative vs 8+ aggressive transfers

### Force Delete Safety
- **Problem**: Delete files when backup logs unavailable
- **Solution**: Triple confirmation system (warning → count → typed "DELETE X FILES")
- **Usage**: `mdump delete --force` (interactive) / `--auto delete --force` (automated)

### Media Validation System
- **Problem**: Verify media files before backup to prevent corrupted imports
- **Solution**: File-type-specific validation (`src/validation.rs`)
  - **FFprobe**: Videos with stream/duration/corruption checks
  - **Custom Commands**: Arbitrary validation (ImageMagick, pdfinfo, etc.)
  - **Pattern Matching**: File-type-specific validators
  - **Performance**: Parallel execution with timeouts

## Configuration

**Validation Example**:
```toml
[source.validation]
enabled = true

# Videos
[source.validation.ffprobe_validation]
file_patterns = ["*.mp4", "*.mov"]
required_streams = ["video"]

# Images  
[[source.validation.custom_commands]]
name = "image_validation"
command = "identify"
args = ["{file_path}"]
file_patterns = ["*.jpg", "*.png"]
```

## Architecture

**Processing Pipeline**: Detection → Collection → Validation → Template Processing → Backup → Logging → Optional Deletion

**Template Variables**:
- Global: `{media_name}`, `{hostname}`, `{uuid}`, `{content_hash}`  
- Time: `{yyyy}`, `{mm}`, `{dd}` (file-specific)
- File: `{original_name}`, `{name}`, `{ext}`

## Quality
- **Clippy fixes**: Default implementations, simplified patterns, removed unused code
- **Tests**: 29 total tests passing, including rclone optimization and integration tests

## Performance
- **Large files**: 4 transfers, single-threaded streams
- **Small files**: CPU-count transfers, 4 streams  
- **Analysis**: Size distribution determines optimal settings

## Security
- **Deletion**: Multiple confirmations, clear warnings, auto mode bypass
- **Validation**: Regex patterns, path sanitization, hash collision detection

## Development Patterns
- **Error handling**: `Result<T>` with graceful degradation
- **Async**: `BoxFuture`, tokio file I/O, concurrent execution
- **CLI**: Subcommands (`backup`, `delete`), interactive/auto modes

## Future Enhancements
- Resume transfers, bandwidth throttling, advanced deduplication
- Progress bars, parallel uploads, plugin system

## Key Commands
```bash
cargo test && cargo clippy && cargo fmt  # Development
mdump backup --dry-run                   # Preview
mdump delete --force                     # Force delete
mdump --auto backup                      # Automated
```

**Host Config**:
```toml
[destinations.photoprism]
path = "/path/to/destination"
name_template = "{yyyy}{mm}{dd}_{content_hash_short}.{ext}"

[destinations.photoprism.processing]
directory_structure = "{yyyy}/{mm}"
```

**Media Config**:
```toml
[source]
name = "My Camera - 256GB"
paths = ["DCIM/DJI_001"]
file_filters = ["*.MP4"]

[source.deletion]
delete_imported_files = true
```

## Validation Examples

**Photography**:
```toml
[[source.validation.custom_commands]]
command = "dcraw"
args = ["-i", "{file_path}"]
file_patterns = ["*.cr2", "*.nef"]
```

**Video Production**:
```toml
[source.validation.ffprobe_validation]
file_patterns = ["*.mov", "*.mp4"]
required_streams = ["video"]
check_corruption = true
```

## Key Learnings
1. Separate backup and deletion for reliability
2. Analyze file characteristics before processing  
3. Multiple safety confirmations prevent data loss
4. File-type-specific validation improves accuracy
5. Comprehensive testing catches regressions