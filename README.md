# mdump - Configurable Media Backup Tool

A Rust CLI tool that provides automated backup of removable media using rclone, with flexible configuration, template-based naming, and robust safety features.

## Features

- **Dual Configuration System**: Media defines what to backup, host defines where and how
- **Template-Based Naming**: Flexible file and directory naming with variables
- **Rclone Integration**: Supports any rclone remote (Google Drive, S3, etc.)  
- **Automatic Remote Setup**: Configures missing rclone remotes interactively
- **High Performance**: CPU-based parallel transfers with direct rclone output
- **File Processing Options**: Folder flattening, hash-based renaming
- **Safety Features**: Integrity verification, confirmed deletion, dry-run mode
- **Cross-Platform**: Works on macOS, Linux, and Windows

## Installation

### Prerequisites

- [Rust](https://rustup.rs/) (latest stable)
- [rclone](https://rclone.org/downloads/) installed and accessible in PATH

### Build from Source

```bash
git clone https://github.com/jh247247/mdump.git
cd mdump
cargo build --release
```

The binary will be available at `target/release/mdump`.

## Configuration

### 1. Host Configuration

Create `~/.config/mdump/host_config.toml` (or specify with `--config`):

On macOS, the default is `~/Library/Application Support/mdump/host_config.toml`.
The command prints the configuration path it loads.

```toml
[destinations.photos]
path = "imported_photos"
rclone_remote = "gdrive_photos"  
name_template = "{media_name}_{date}_{content_hash_short}"

[destinations.photos.processing]
flatten_folders = false
directory_structure = "{yyyy}/{mm}/{media_name}"

[remotes.gdrive_photos]
type = "drive"
scope = "drive"

[rclone]
bandwidth_limit = "20M"
# transfers defaults to CPU core count (16 cores = 16 parallel transfers)
additional_flags = ["--progress", "--checksum"]

[security]
verify_integrity = true
require_confirmation_before_delete = true
```

### 2. Media Configuration

Place `mdump_source.toml` in the root of your removable media:

```toml
[source]
name = "Family_Photos_2024"
description = "Summer vacation and family events"
paths = ["DCIM/Camera/", "Documents/", "Videos/"]
file_filters = ["*.jpg", "*.mp4", "*.pdf"]
exclude_patterns = [".DS_Store", "Thumbs.db"]
```

## Template Variables

### File Templates (`name_template`)
- `{original_name}` - Original filename with extension
- `{name}` - Filename without extension  
- `{ext}` - File extension
- `{content_hash}` - SHA256 hash of file content (first 1MB)
- `{content_hash_short}` - First 8 characters of content hash

### Directory Templates (`directory_structure`)
- `{media_name}` - From media config
- `{description}` - From media config
- `{yyyy}`, `{mm}`, `{dd}` - Date components
- `{hostname}` - Current hostname
- `{uuid}` - Random UUID
- `{original_path}` - Original relative path from media

## Usage

### Basic Backup
```bash
# Scan for media and backup interactively
mdump backup

# Backup to specific destination
mdump backup --destination photos

# Preview operations without executing
mdump backup --dry-run
```

### Remote Management
```bash
# List configured remotes
mdump remotes list

# Test remote connectivity
mdump remotes test gdrive_photos

# Setup missing remotes
mdump remotes setup

# Import existing rclone config
mdump remotes import
```

## Example Workflows

### 1. Photo Import with Date Organization
```toml
[destinations.photos]
name_template = "{original_name}"
directory_structure = "{yyyy}/{mm}/{media_name}"
flatten_folders = false
```
Result: `2024/08/Family_Photos/IMG_001.jpg`

### 2. Hash-Based Deduplication  
```toml
[destinations.archive]
name_template = "{content_hash}.{ext}"
directory_structure = "archive/{yyyy}"
flatten_folders = true
```
Result: `archive/2024/a1b2c3d4e5f6789...abc.jpg`

### 3. Structured Backup with Metadata
```toml
[destinations.organized]
name_template = "{media_name}_{content_hash_short}_{original_name}"
directory_structure = "{hostname}/{yyyy}/{mm}"
flatten_folders = false
```
Result: `macbook/2024/08/DCIM/Family_Photos_a1b2c3d4_IMG_001.jpg`

## Safety Features

- **Integrity Verification**: Uses rclone's checksum verification
- **Confirmed Deletion**: Multi-step confirmation before deleting source files
- **Dry Run Mode**: Preview all operations before execution
- **Error Handling**: Comprehensive error reporting and recovery
- **Path Sanitization**: Automatic cleanup of problematic characters

## Supported Platforms

- **macOS**: Scans `/Volumes` for removable media
- **Linux**: Scans `/media` and `/mnt` for mounted devices
- **Windows**: Scans drive letters for removable media

## Examples

See the `examples/` directory for:
- `mdump_source.toml` - Basic media configuration
- `host_config.toml` - Standard host configuration  
- `advanced_config.toml` - Advanced configuration with multiple destinations

## Contributing

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Add tests for new functionality
5. Submit a pull request

## License

This project is licensed under the MIT License - see the LICENSE file for details.
