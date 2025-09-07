pub mod config;
pub mod media;
pub mod operations;
pub mod rclone;
pub mod remotes;
pub mod templates;
pub mod validation;

pub use config::{HostConfig, MediaConfig};
pub use media::MediaDetector;
pub use operations::FileProcessor;
pub use rclone::RcloneWrapper;
pub use remotes::RemoteManager;
pub use templates::TemplateProcessor;

pub type Result<T> = anyhow::Result<T>;
