#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "macos")]
mod macos;
mod common;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "windows")]
pub use windows::*;
#[cfg(target_os = "macos")]
pub use macos::*;

// Common function to create the platform-specific impl
pub fn create_pty_impl(cmd: &crate::pty::Command, size: portable_pty::PtySize) -> Result<std::sync::Arc<dyn crate::pty::PtyTrait>, Box<dyn std::error::Error + Send + Sync>> {
    // This will use the platform-specific PtyImpl from the cfg above
    PtyImpl::new(cmd, size).map(|arc| arc as std::sync::Arc<dyn crate::pty::PtyTrait>)
}