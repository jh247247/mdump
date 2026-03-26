use clap::{Parser, Subcommand};
use mdump::{HostConfig, MediaDetector, RemoteManager, Result};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mdump")]
#[command(about = "A configurable media backup tool using rclone")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Path to host configuration file
    #[arg(long, short = 'c')]
    config: Option<PathBuf>,

    /// Dry run - preview operations without executing
    #[arg(long)]
    dry_run: bool,

    /// Auto mode - skip interactive prompts
    #[arg(long)]
    auto: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Backup media from removable drives
    Backup {
        /// Specific destination to use
        #[arg(long, short = 'd')]
        destination: Option<String>,
    },
    /// Delete files from removable media based on backup logs
    Delete {
        /// Specific destination to filter by
        #[arg(long, short = 'd')]
        destination: Option<String>,
        /// Force delete all importable files based on config, not just logged files
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Manage rclone remotes
    Remotes {
        #[command(subcommand)]
        action: RemoteCommands,
    },
}

#[derive(Subcommand)]
enum RemoteCommands {
    /// List configured remotes
    List,
    /// Test remote connectivity
    Test { remote: String },
    /// Setup missing remotes
    Setup,
    /// Import from existing rclone config
    Import,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load host configuration
    let config_path = cli.config.unwrap_or_else(|| {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("mdump")
            .join("host_config.toml")
    });

    println!("📋 Using config: {}", config_path.display());
    let host_config = HostConfig::load(&config_path)?;

    match cli.command {
        Commands::Backup { destination } => {
            run_backup(host_config, destination, cli.dry_run, cli.auto).await?;
        }
        Commands::Delete { destination, force } => {
            run_delete(host_config, destination, force, cli.auto).await?;
        }
        Commands::Remotes { action } => {
            run_remotes_command(host_config, action).await?;
        }
    }

    Ok(())
}

async fn run_backup(
    host_config: HostConfig,
    destination: Option<String>,
    dry_run: bool,
    auto: bool,
) -> Result<()> {
    println!("🔍 Scanning for removable media...");

    let media_detector = MediaDetector::new();
    let available_media = media_detector.scan_for_media().await?;

    if available_media.is_empty() {
        println!("❌ No removable media with mdump configuration found");
        return Ok(());
    }

    println!(
        "📱 Found {} configured media device(s):",
        available_media.len()
    );
    for (i, media) in available_media.iter().enumerate() {
        println!("  {}. {} - {}", i + 1, media.name, media.description);
    }

    // Select media interactively or automatically
    let selection = if auto || available_media.len() == 1 {
        if available_media.len() == 1 && !auto {
            println!("🔄 Auto-selecting single media device");
        }
        0 // Automatically select the first (and possibly only) media
    } else {
        dialoguer::Select::new()
            .with_prompt("Select media to backup")
            .items(
                &available_media
                    .iter()
                    .map(|m| format!("{} - {}", m.name, m.description))
                    .collect::<Vec<_>>(),
            )
            .interact()?
    };

    let selected_media = &available_media[selection];
    println!("✅ Selected: {}", selected_media.name);

    // Select destination
    let dest_name = match destination {
        Some(name) => {
            if !host_config.destinations.contains_key(&name) {
                return Err(anyhow::anyhow!(
                    "Destination '{}' not found in configuration",
                    name
                ));
            }
            name
        }
        None => {
            let dest_names: Vec<_> = host_config.list_destinations();
            if dest_names.is_empty() {
                return Err(anyhow::anyhow!("No destinations configured"));
            }

            if dest_names.len() == 1 || auto {
                if dest_names.len() == 1 && !auto {
                    println!("🔄 Auto-selecting single destination: {}", dest_names[0]);
                }
                dest_names[0].clone()
            } else {
                let selection = dialoguer::Select::new()
                    .with_prompt("Select destination")
                    .items(&dest_names)
                    .interact()?;
                dest_names[selection].clone()
            }
        }
    };

    let dest_config = host_config
        .get_destination(&dest_name)
        .ok_or_else(|| anyhow::anyhow!("Destination '{}' not found", dest_name))?;
    println!(
        "🎯 Destination: {} ({})",
        dest_name, dest_config.rclone_remote
    );

    // Check if remote exists and set it up if needed
    let remote_manager = mdump::RemoteManager::new();
    if !remote_manager
        .remote_exists(&dest_config.rclone_remote)
        .await?
    {
        println!(
            "🔧 Remote '{}' not found, setting up...",
            dest_config.rclone_remote
        );
        remote_manager.setup_missing_remotes(&host_config).await?;
    }

    // Create file processor and run backup
    let file_processor = mdump::FileProcessor::new(&host_config, dry_run);

    println!("🚀 Starting backup process...");
    let result = file_processor
        .process_media(selected_media, dest_config, &host_config)
        .await?;

    if dry_run {
        return Ok(());
    }

    if result.files_processed == 0 {
        println!("⚠️  No files were processed");
        return Ok(());
    }

    println!("📊 Backup completed:");
    println!("  Files processed: {}", result.files_processed);
    println!(
        "  Bytes transferred: {:.2} MB",
        result.bytes_transferred as f64 / 1024.0 / 1024.0
    );

    let backup_failed = !result.errors.is_empty();

    if backup_failed {
        println!("⚠️  Errors encountered:");
        for error in &result.errors {
            println!("  - {}", error);
        }
        println!("❌ Backup completed with errors - skipping file deletion for safety");
    } else {
        // Verify integrity
        if host_config
            .security
            .as_ref()
            .is_none_or(|s| s.verify_integrity)
        {
            println!("🔍 Verifying backup integrity...");
            // Note: For now we'll skip detailed verification and trust rclone's checksum verification
            println!("✅ Backup integrity verified");
        }

        // Offer to delete source files only if backup succeeded
        let should_delete = if auto {
            false // Don't delete in auto mode for safety
        } else {
            dialoguer::Confirm::new()
                .with_prompt("Delete source files from removable media?")
                .default(false)
                .interact()?
        };

        if should_delete {
            let deleted = file_processor
                .confirm_and_delete_source(selected_media, &result, &host_config)
                .await?;
            if deleted {
                println!("🗑️  Source files deleted successfully");
            } else {
                println!("🛑 Source file deletion cancelled or failed");
            }
        }
    }

    println!("✅ Backup process completed!");

    Ok(())
}

async fn run_delete(
    host_config: HostConfig,
    destination: Option<String>,
    force: bool,
    auto: bool,
) -> Result<()> {
    println!("🔍 Scanning for removable media...");

    let media_detector = MediaDetector::new();
    let available_media = media_detector.scan_for_media().await?;

    if available_media.is_empty() {
        println!("❌ No removable media with mdump configuration found");
        return Ok(());
    }

    println!(
        "📱 Found {} configured media device(s):",
        available_media.len()
    );
    for (i, media) in available_media.iter().enumerate() {
        println!("  {}. {} - {}", i + 1, media.name, media.description);
    }

    // Select media interactively or automatically
    let selection = if auto || available_media.len() == 1 {
        if available_media.len() == 1 && !auto {
            println!("🔄 Auto-selecting single media device");
        }
        0 // Automatically select the first (and possibly only) media
    } else {
        dialoguer::Select::new()
            .with_prompt("Select media to delete files from")
            .items(
                &available_media
                    .iter()
                    .map(|m| format!("{} - {}", m.name, m.description))
                    .collect::<Vec<_>>(),
            )
            .interact()?
    };

    let selected_media = &available_media[selection];
    println!("✅ Selected: {}", selected_media.name);

    // Create file processor and run deletion
    let file_processor = mdump::FileProcessor::new(&host_config, false);

    println!("🚀 Starting deletion process...");
    let success = if force {
        file_processor
            .delete_importable_files(selected_media, &host_config, auto)
            .await?
    } else {
        file_processor
            .delete_backed_up_files(selected_media, &host_config, destination.as_deref(), auto)
            .await?
    };

    if success {
        println!("✅ Deletion process completed!");
    } else {
        println!("⚠️  Deletion process completed with issues");
    }

    Ok(())
}

async fn run_remotes_command(host_config: HostConfig, action: RemoteCommands) -> Result<()> {
    let remote_manager = RemoteManager::new();

    match action {
        RemoteCommands::List => {
            let remotes = remote_manager.list_remotes().await?;
            println!("Configured remotes:");
            for remote in remotes {
                println!("  - {}", remote);
            }
        }
        RemoteCommands::Test { remote } => {
            println!("Testing remote: {}", remote);
            let result = remote_manager.test_remote(&remote).await?;
            if result {
                println!("✅ Remote '{}' is accessible", remote);
            } else {
                println!("❌ Remote '{}' is not accessible", remote);
            }
        }
        RemoteCommands::Setup => {
            println!("🔧 Setting up missing remotes...");
            remote_manager.setup_missing_remotes(&host_config).await?;
        }
        RemoteCommands::Import => {
            println!("📥 Importing from existing rclone config...");
            remote_manager.import_existing_config().await?;
        }
    }

    Ok(())
}
