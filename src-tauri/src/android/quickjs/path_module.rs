//! Node.js `path` module implementation for QuickJS.
//!
//! Implements the subset of path operations needed by TiddlyWiki:
//! - join, resolve, dirname, basename, extname, normalize, sep, delimiter

use rquickjs::{Ctx, Function, Object, Result};
use rquickjs::function::{Opt, Rest};

/// Register the `path` module in the QuickJS context.
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    let globals = ctx.globals();

    // Create the path module object
    let path = Object::new(ctx.clone())?;

    // path.sep - Path segment separator
    #[cfg(windows)]
    path.set("sep", "\\")?;
    #[cfg(not(windows))]
    path.set("sep", "/")?;

    // path.delimiter - Path list delimiter (for PATH environment variable)
    #[cfg(windows)]
    path.set("delimiter", ";")?;
    #[cfg(not(windows))]
    path.set("delimiter", ":")?;

    // path.join(...paths) - Join path segments
    path.set("join", Function::new(ctx.clone(), path_join)?)?;

    // path.resolve(...paths) - Resolve to absolute path
    path.set("resolve", Function::new(ctx.clone(), path_resolve)?)?;

    // path.dirname(path) - Get directory name
    path.set("dirname", Function::new(ctx.clone(), path_dirname)?)?;

    // path.basename(path, ext?) - Get base name
    path.set("basename", Function::new(ctx.clone(), path_basename)?)?;

    // path.extname(path) - Get extension
    path.set("extname", Function::new(ctx.clone(), path_extname)?)?;

    // path.normalize(path) - Normalize path
    path.set("normalize", Function::new(ctx.clone(), path_normalize)?)?;

    // path.isAbsolute(path) - Check if path is absolute
    path.set("isAbsolute", Function::new(ctx.clone(), path_is_absolute)?)?;

    // path.parse(path) - Parse path into components
    path.set("parse", Function::new(ctx.clone(), path_parse)?)?;

    // Register as global require result
    // TiddlyWiki uses: var path = require("path");
    globals.set("__path_module", path)?;

    Ok(())
}

/// Join path segments together.
fn path_join(args: Rest<String>) -> String {
    use std::path::PathBuf;

    let mut result = PathBuf::new();
    for arg in args.0 {
        if arg.starts_with('/') || (arg.len() >= 2 && arg.chars().nth(1) == Some(':')) {
            // Absolute path - reset
            result = PathBuf::from(&arg);
        } else {
            result.push(&arg);
        }
    }

    // Normalize separators for the platform
    normalize_separators(&result.to_string_lossy())
}

/// Resolve path segments to an absolute path.
fn path_resolve(args: Rest<String>) -> String {
    use std::path::PathBuf;

    let mut result = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));

    for arg in args.0 {
        if arg.starts_with('/') || (arg.len() >= 2 && arg.chars().nth(1) == Some(':')) {
            // Absolute path - reset
            result = PathBuf::from(&arg);
        } else {
            result.push(&arg);
        }
    }

    // Canonicalize to resolve . and ..
    match result.canonicalize() {
        Ok(canonical) => normalize_separators(&canonical.to_string_lossy()),
        Err(_) => normalize_separators(&result.to_string_lossy()),
    }
}

/// Get the directory name of a path.
fn path_dirname(path: String) -> String {
    use std::path::Path;

    Path::new(&path)
        .parent()
        .map(|p| normalize_separators(&p.to_string_lossy()))
        .unwrap_or_else(|| ".".to_string())
}

/// Get the base name of a path, optionally removing an extension.
fn path_basename(path: String, ext: Opt<String>) -> String {
    use std::path::Path;

    let name = Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // Remove extension if provided
    if let Some(ext) = ext.0 {
        if name.ends_with(&ext) {
            return name[..name.len() - ext.len()].to_string();
        }
    }

    name
}

/// Get the extension of a path.
fn path_extname(path: String) -> String {
    use std::path::Path;

    Path::new(&path)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default()
}

/// Normalize a path (resolve . and ..).
fn path_normalize(path: String) -> String {
    use std::path::PathBuf;

    let mut result = Vec::new();
    let is_absolute = path.starts_with('/') || (path.len() >= 2 && path.chars().nth(1) == Some(':'));

    for component in path.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => {
                if !result.is_empty() && result.last() != Some(&"..") {
                    result.pop();
                } else if !is_absolute {
                    result.push("..");
                }
            }
            c => result.push(c),
        }
    }

    let normalized = result.join("/");
    if is_absolute {
        if path.starts_with('/') {
            format!("/{}", normalized)
        } else {
            // Windows absolute path
            normalized
        }
    } else if normalized.is_empty() {
        ".".to_string()
    } else {
        normalized
    }
}

/// Check if a path is absolute.
fn path_is_absolute(path: String) -> bool {
    path.starts_with('/') || (path.len() >= 2 && path.chars().nth(1) == Some(':'))
}

/// Parse a path into its components.
fn path_parse(ctx: Ctx<'_>, path: String) -> Result<Object<'_>> {
    use std::path::Path;

    let _p = Path::new(&path);
    let obj = Object::new(ctx)?;

    // root
    let root = if path.starts_with('/') {
        "/"
    } else if path.len() >= 3 && path.chars().nth(1) == Some(':') && path.chars().nth(2) == Some('\\') {
        &path[0..3]
    } else {
        ""
    };
    obj.set("root", root)?;

    // dir
    obj.set("dir", path_dirname(path.clone()))?;

    // base
    obj.set("base", path_basename(path.clone(), Opt(None)))?;

    // ext
    obj.set("ext", path_extname(path.clone()))?;

    // name (base without extension)
    let base = path_basename(path.clone(), Opt(None));
    let ext = path_extname(path);
    let name = if !ext.is_empty() && base.ends_with(&ext) {
        base[..base.len() - ext.len()].to_string()
    } else {
        base
    };
    obj.set("name", name)?;

    Ok(obj)
}

/// Normalize path separators (always use forward slashes internally).
fn normalize_separators(path: &str) -> String {
    path.replace('\\', "/")
}
