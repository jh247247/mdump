# mdump2 Development Session Learnings

This file captures key insights, solutions, and lessons learned from the Claude Code development session for mdump2.

## Project Overview

mdump2 is a configurable media backup tool using rclone, designed for backing up photos/videos from removable media (SD cards, USB drives) to cloud storage destinations with intelligent file organization and deduplication.

## Major Issues Solved

### 1. File Deletion Not Working After Backup

**Problem**: Files weren't being deleted after successful backups because:
- Rclone was spending excessive time checking checksums for existing files
- Backup process never completed successfully, so deletion logic never triggered
- No way to delete files from previous backup sessions

**Solution**: 
- Added backup logging system (`BackupLog` structs) to track successful transfers
- Created independent `delete` subcommand that works from logs, not active sessions
- Separated backup and deletion workflows for better reliability

**Key Files**:
- `src/operations.rs`: Added `save_backup_log()`, `load_backup_logs()`, `delete_backed_up_files()`
- Log location: `~/Library/Application Support/mdump/logs/{media_name}.json`

### 2. File Naming Using Current Date Instead of File Date

**Problem**: Files were named with backup date instead of original creation date.

**Solution**: Modified `set_file_variables()` in `src/templates.rs` to:
- Extract file metadata using `std::fs::metadata()`
- Use creation time with fallback to modification time
- Override global date variables with file-specific ones

**Before**: `20250907_40c07fa2.mp4` (backup date)
**After**: `20250906_40c07fa2.mp4` (file creation date)

### 3. Inefficient rclone Parallel Transfers

**Problem**: Using CPU core count for all transfers caused disk thrashing with large video files.

**Solution**: Intelligent transfer optimization in `src/rclone.rs`:
- **Large files** (>100MB or >50% large): Max 4 transfers, single-threaded streams
- **Small files**: Full CPU utilization, multi-threaded streams up to 4
- File analysis determines optimal settings automatically

**Performance Impact**: 19 video files averaging 4GB each now use 4 conservative transfers instead of 8+ aggressive ones.

### 4. Enhanced Safety for Force Delete

**Problem**: Need to delete files based on configuration when backup logs unavailable.

**Solution**: Added `--force` flag with triple confirmation:
1. Understanding warning about no backup verification
2. Exact file count confirmation  
3. Typed confirmation requiring "DELETE X FILES"

**Usage**:
```bash
mdump delete --force                    # Interactive with triple confirmation
mdump --auto delete --force            # Automated (skips confirmation)
```

### 5. Media Validation System

**Problem**: Need to verify media files are valid before importing to prevent corrupted files from being backed up.

**Solution**: Comprehensive validation system with ffprobe and custom commands in `src/validation.rs`:

**Features**:
- **FFprobe Integration**: Validates video/audio files using ffprobe
  - File-type-specific patterns (`["*.mp4", "*.mov", "*.avi"]`)
  - Required stream validation (video, audio)
  - Duration constraints (min/max)
  - Corruption detection
  - Custom ffprobe binary paths
- **Custom Commands**: Execute arbitrary validation commands
  - File-type-specific patterns for targeted validation
  - Placeholder substitution (`{file_path}`)
  - Expected exit codes
  - Per-command timeouts
- **Flexible Configuration**: Skip vs fail on validation errors
- **Performance**: Parallel validation with configurable timeouts

**Configuration Example**:
```toml
[source.validation]
enabled = true
skip_on_validation_failure = true
max_validation_time_seconds = 30

# FFprobe for video files only
[source.validation.ffprobe_validation]
enabled = true
file_patterns = ["*.mp4", "*.mov", "*.avi", "*.mkv"]
required_streams = ["video"]
min_duration_seconds = 1.0
check_corruption = false

# Image validation with ImageMagick
[[source.validation.custom_commands]]
name = "image_validation"
command = "identify"
args = ["{file_path}"]
file_patterns = ["*.jpg", "*.jpeg", "*.png", "*.tiff"]
expected_exit_code = 0
timeout_seconds = 10

# PDF validation
[[source.validation.custom_commands]]
name = "pdf_validation"
command = "pdfinfo"
args = ["{file_path}"]
file_patterns = ["*.pdf"]
expected_exit_code = 0
timeout_seconds = 5
```

**Key Files**:
- `src/validation.rs`: Core validation engine
- `src/config.rs`: ValidationConfig, FfprobeConfig, CustomValidationCommand structs
- `src/operations.rs`: Integration into file processing pipeline

## Technical Architecture Insights

### Configuration System
- **Host Config**: `~/.config/mdump/host_config.toml` or `~/Library/Application Support/mdump/host_config.toml`
- **Media Config**: `mdump_source.toml` on removable media root
- **Deletion Config**: Within media config under `[source.deletion]`

### File Processing Pipeline
1. **Media Detection**: Scan for removable drives with mdump config
2. **File Collection**: Recursive scan with filter/exclude patterns
3. **Template Processing**: Generate destination paths using variables
4. **Symbolic Link Strategy**: Create temp directory structure, bulk copy
5. **Backup Logging**: Record successful transfers for future deletion
6. **Optional Deletion**: Based on logs or force mode

### Template Variables
- **Global**: `{media_name}`, `{hostname}`, `{uuid}`, `{content_hash}`
- **Time**: `{yyyy}`, `{mm}`, `{dd}`, `{date}`, `{time}` (now file-specific)
- **File**: `{original_name}`, `{name}`, `{ext}`, `{original_path}`

## Code Quality Improvements

### Clippy Fixes Applied
- Added `Default` implementations for `MediaDetector` and `RemoteManager`
- Simplified `map_or` calls to `is_none_or` where appropriate
- Fixed collapsible match patterns
- Removed unused `delete_path_recursive` method
- Used `derive(Default)` instead of manual implementations
- Fixed needless borrows and redundant closures

### Testing Coverage
- 23 unit tests covering all major functionality
- 5 specific tests for rclone optimization logic
- Integration tests for end-to-end workflows
- All tests passing after refactoring

## Performance Optimizations

### rclone Transfer Settings

**Large Files Strategy** (>50% files >100MB OR avg >50MB):
- `--transfers=4` (max)
- `--checkers=8` (2x transfers)
- `--multi-thread-streams=1` (sequential IO)

**Small Files Strategy**:
- `--transfers={cpu_count}` (full utilization)  
- `--checkers={cpu_count}` (match transfers)
- `--multi-thread-streams=4` (parallel throughput)

### File Analysis Algorithm
```rust
fn calculate_optimal_transfers(total_files: u64, total_size: u64, large_files: u64, cpu_count: u32) -> (u32, u32, u32)
```
- Analyzes file size distribution before transfer
- Provides console feedback about detected characteristics
- Graceful fallback to safe defaults on analysis failure

## Security Considerations

### Deletion Safety
- Multiple confirmation layers for destructive operations
- Clear warnings about backup verification status
- Auto mode bypasses confirmations (use carefully)
- Detailed file listings before deletion
- Separation of normal vs force delete workflows

### Configuration Validation
- File filter patterns validated as regex
- Path sanitization for cross-platform compatibility
- Hash collision detection with progressive reading
- Integrity verification using rclone checksums

## Development Best Practices Demonstrated

### Error Handling
- Comprehensive `Result<T>` usage throughout
- Graceful degradation when non-critical features fail
- Clear error messages with context
- Recovery strategies for common failure modes

### Async/Await Patterns
- `BoxFuture` for complex recursive operations
- Proper async file I/O with tokio
- Concurrent tool execution where beneficial
- Background process management

### CLI Design
- Intuitive subcommand structure (`backup`, `delete`, `remotes`)
- Consistent flag naming (`--force`, `--auto`, `--destination`)
- Helpful error messages and usage guidance
- Interactive vs automated operation modes

## Future Enhancement Opportunities

### Features to Consider
- Resume interrupted transfers
- Bandwidth throttling per destination
- Advanced deduplication strategies
- Multi-media type handling (RAW, LOG files)
- Remote integrity verification
- Incremental backup detection

### Technical Improvements  
- Real-time progress bars for individual files
- Parallel destination uploads
- Configuration file templates/presets
- Plugin system for custom processors
- Database backend for large-scale logging

## Key Commands for Development

```bash
# Development workflow
cargo test                              # Run all tests
cargo clippy                           # Lint checking  
cargo fmt                              # Code formatting
cargo build --release                 # Production build

# Usage examples
./target/release/mdump --help          # Show all options
./target/release/mdump backup --dry-run # Preview operations
./target/release/mdump delete --force  # Force delete with confirmation
./target/release/mdump --auto backup   # Automated backup
```

## Configuration Examples

### Host Config (Production)
```toml
[destinations.photoprism]
path = "/Volumes/Tower/photoprism"
rclone_remote = "local"
name_template = "{yyyy}{mm}{dd}_{content_hash_short}.{ext}"

[destinations.photoprism.processing]
flatten_folders = false
directory_structure = "{yyyy}/{mm}"

[security]
verify_integrity = true
require_confirmation_before_delete = true
```

### Media Config (SD Card)
```toml
[source]
name = "Osmo pocket 3 - 256"
description = "Small osmo pocket 3 sd card"
paths = ["DCIM/DJI_001"]
file_filters = ["*.MP4"]
exclude_patterns = [".DS_Store", "Thumbs.db", "*.tmp", "._*"]

[source.deletion]
delete_imported_files = true
extra_file_patterns = []
```

## Lessons Learned

1. **Separation of Concerns**: Backup and deletion should be independent operations for reliability
2. **File Analysis First**: Understanding data characteristics before processing prevents performance issues
3. **Progressive Safety**: Multiple confirmation layers prevent accidental data loss
4. **Template Flexibility**: Rich variable system enables complex file organization patterns
5. **Error Recovery**: Graceful fallbacks and clear error messages improve user experience
6. **Test Coverage**: Comprehensive testing catches regressions during refactoring
7. **Code Quality**: Regular linting and formatting maintains professional standards

This documentation should enable future development sessions to quickly understand the architecture, solutions implemented, and best practices established during this development cycle.