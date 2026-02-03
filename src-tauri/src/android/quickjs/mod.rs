//! QuickJS runtime for running TiddlyWiki on Android.
//!
//! This module provides a lightweight JavaScript runtime using QuickJS
//! to run TiddlyWiki without requiring Node.js. It implements the minimal
//! subset of Node.js APIs needed by TiddlyWiki's boot.js.
//!
//! ## Architecture
//!
//! - Custom `fs` module that uses Android SAF for file operations
//! - Custom `path` module for path manipulation
//! - TiddlyWiki boot.js loaded and executed in QuickJS context
//!
//! ## Usage
//!
//! ```rust,ignore
//! let runtime = TiddlyWikiRuntime::new()?;
//! runtime.load_wiki("/path/to/wiki")?;
//! let html = runtime.render()?;
//! ```

#![cfg(target_os = "android")]
// Allow dead code in this module - it's prepared for future integration
#![allow(dead_code)]

mod fs_module;
mod path_module;
mod runtime;

pub use runtime::TiddlyWikiRuntime;

/// Initialize the QuickJS runtime for TiddlyWiki.
/// Called during app startup on Android.
pub fn init() -> Result<(), String> {
    eprintln!("[TiddlyDesktop] QuickJS runtime initialized");
    Ok(())
}
