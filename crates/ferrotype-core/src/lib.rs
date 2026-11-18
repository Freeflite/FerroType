//! FerroType's logic: getting text ready to type, the typing loop, hotkeys, the history and the
//! settings. Nothing here calls Windows or uses `unsafe`, so all of it can be tested on its own;
//! `ferrotype-sys` does the Win32 work and the `ferrotype` app wires the two together.
#![forbid(unsafe_code)]

pub mod format;
pub mod history;
pub mod hotkey;
pub mod rng;
pub mod settings;
pub mod text;
pub mod typing;
