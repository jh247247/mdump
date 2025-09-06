pub mod config;
pub mod remotes;
pub mod templates;
pub mod operations;
pub mod rclone;
pub mod media;

pub use config::{HostConfig, MediaConfig};
pub use remotes::RemoteManager;
pub use templates::TemplateProcessor;
pub use operations::FileProcessor;
pub use rclone::RcloneWrapper;
pub use media::MediaDetector;

pub type Result<T> = anyhow::Result<T>;