//! Thin Win32 layer for FerroType: keystrokes, hotkeys, the clipboard, DPAPI and window chrome.
//!
//! This is the only crate in the workspace allowed to use `unsafe`. The rules:
//!
//! - every unsafe block is a single operation with a `SAFETY:` comment (enforced by clippy);
//! - every buffer Windows hands back is bounds-checked before anything is read from it;
//! - handles are owned (`OwnedHandle`, or a guard that releases them) so they can't leak.
#![cfg(windows)]

pub mod clipboard;
pub mod dpapi;
mod error;
pub mod input;
pub mod system;
pub mod watcher;
pub mod window;

pub use error::{Error, Result};
