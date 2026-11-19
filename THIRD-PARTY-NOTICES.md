# Third-party notices

## Slint

The UI is built with [Slint](https://slint.dev), used under the Slint Royalty-free Desktop,
Mobile, and Web Applications License. As that license requires, FerroType's Settings page (About)
shows the `AboutSlint` attribution.

## Rust crates

FerroType's other dependencies are Rust crates under permissive licenses (MIT, Apache-2.0, BSD,
Zlib, Unicode and similar); `cargo deny check licenses` enforces the list in `deny.toml`. Their
license texts are in each crate's source on [crates.io](https://crates.io). The security-key
lock's encryption comes from the [RustCrypto](https://github.com/RustCrypto) project's `aes-gcm`,
`hkdf` and `sha2` crates (MIT or Apache-2.0).

## Ferrotop

The window chrome code (`crates/ferrotype-sys/src/window.rs`), the Start menu shortcut and
install code (`shortcut.rs`, `install.rs`), the build and release scripts and the hardening setup
come from [Ferrotop](https://github.com/Freeflite/Ferrotop), by the same author, under the same
MIT license.

## App icon

The icon in `crates/ferrotype/assets/` is FreeFlite's own artwork, used with permission of its
owner.
