// rust-pty/src/platform/windows/pty_impl.rs
use super::super::common::PtyImpl as CommonPtyImpl; // reuse the common struct/impl
pub use CommonPtyImpl; // expose the same name the rest of the crate expects
