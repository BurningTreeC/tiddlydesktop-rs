//! Node.js `fs` module implementation for QuickJS.
//!
//! Implements the subset of fs operations needed by TiddlyWiki:
//! - readFileSync, writeFileSync, existsSync, statSync, readdirSync, mkdirSync, unlinkSync
//!
//! All operations use Android SAF for file access via content:// URIs.

use rquickjs::{Ctx, Function, Object, Result, Value};
use rquickjs::function::Opt;
use crate::android::saf;

/// Register the `fs` module in the QuickJS context.
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    let globals = ctx.globals();

    // Create the fs module object
    let fs = Object::new(ctx.clone())?;

    // Synchronous file operations (what TiddlyWiki primarily uses)
    fs.set("readFileSync", Function::new(ctx.clone(), read_file_sync)?)?;
    fs.set("writeFileSync", Function::new(ctx.clone(), write_file_sync)?)?;
    fs.set("existsSync", Function::new(ctx.clone(), exists_sync)?)?;
    fs.set("statSync", Function::new(ctx.clone(), stat_sync)?)?;
    fs.set("readdirSync", Function::new(ctx.clone(), readdir_sync)?)?;
    fs.set("mkdirSync", Function::new(ctx.clone(), mkdir_sync)?)?;
    fs.set("unlinkSync", Function::new(ctx.clone(), unlink_sync)?)?;
    fs.set("rmdirSync", Function::new(ctx.clone(), rmdir_sync)?)?;
    fs.set("renameSync", Function::new(ctx.clone(), rename_sync)?)?;
    fs.set("copyFileSync", Function::new(ctx.clone(), copy_file_sync)?)?;

    // Constants
    let constants = Object::new(ctx.clone())?;
    constants.set("F_OK", 0i32)?;  // File exists
    constants.set("R_OK", 4i32)?;  // Read permission
    constants.set("W_OK", 2i32)?;  // Write permission
    constants.set("X_OK", 1i32)?;  // Execute permission
    fs.set("constants", constants)?;

    // Register as global require result
    // TiddlyWiki uses: var fs = require("fs");
    globals.set("__fs_module", fs)?;

    Ok(())
}

/// Read a file synchronously and return its contents.
/// fs.readFileSync(path, options?)
fn read_file_sync<'a>(ctx: Ctx<'a>, path: String, options: Opt<Value<'a>>) -> Result<Value<'a>> {
    // Determine encoding from options
    let encoding = get_encoding_option(&ctx, options.0)?;

    // Read via SAF
    match &encoding {
        Some(enc) if enc == "utf8" || enc == "utf-8" => {
            match saf::read_document_string(&path) {
                Ok(content) => Ok(Value::from_string(rquickjs::String::from_str(ctx, &content)?)),
                Err(e) => Err(make_fs_error(&ctx, "ENOENT", &format!("Failed to read {}: {}", path, e))),
            }
        }
        None => {
            // Return Buffer (as Uint8Array) when no encoding specified
            match saf::read_document_bytes(&path) {
                Ok(bytes) => {
                    let array = rquickjs::TypedArray::<u8>::new(ctx.clone(), bytes)?;
                    Ok(array.into_value())
                }
                Err(e) => Err(make_fs_error(&ctx, "ENOENT", &format!("Failed to read {}: {}", path, e))),
            }
        }
        Some(enc) => {
            Err(make_fs_error(&ctx, "ERR_INVALID_ARG_VALUE", &format!("Unknown encoding: {}", enc)))
        }
    }
}

/// Write data to a file synchronously.
/// fs.writeFileSync(path, data, options?)
fn write_file_sync(ctx: Ctx<'_>, path: String, data: Value<'_>, _options: Opt<Value<'_>>) -> Result<()> {
    // Convert data to string or bytes
    let content = if data.is_string() {
        data.as_string()
            .ok_or_else(|| make_fs_error(&ctx, "ERR_INVALID_ARG_TYPE", "Expected string"))?
            .to_string()?
    } else if let Some(array) = data.as_object().and_then(|o| rquickjs::TypedArray::<u8>::from_object(o.clone()).ok()) {
        // TypedArray (Buffer-like)
        let bytes: Vec<u8> = array.as_bytes().ok_or_else(|| {
            make_fs_error(&ctx, "ERR_INVALID_ARG_TYPE", "Failed to read typed array")
        })?.to_vec();
        String::from_utf8_lossy(&bytes).to_string()
    } else {
        // Try to convert to string
        data.as_string()
            .map(|s| s.to_string())
            .transpose()?
            .unwrap_or_default()
    };

    match saf::write_document_string(&path, &content) {
        Ok(()) => Ok(()),
        Err(e) => Err(make_fs_error(&ctx, "EACCES", &format!("Failed to write {}: {}", path, e))),
    }
}

/// Check if a file exists synchronously.
/// fs.existsSync(path)
fn exists_sync(path: String) -> bool {
    saf::document_exists(&path)
}

/// Get file statistics synchronously.
/// fs.statSync(path)
fn stat_sync(ctx: Ctx<'_>, path: String) -> Result<Object<'_>> {
    if !saf::document_exists(&path) {
        return Err(make_fs_error(&ctx, "ENOENT", &format!("No such file or directory: {}", path)));
    }

    let is_dir = saf::is_directory(&path);
    create_stats_object(&ctx, is_dir)
}

/// Read directory contents synchronously.
/// fs.readdirSync(path)
fn readdir_sync(ctx: Ctx<'_>, path: String) -> Result<Vec<String>> {
    match saf::list_directory(&path) {
        Ok(entries) => Ok(entries),
        Err(e) => Err(make_fs_error(&ctx, "ENOENT", &format!("Failed to read directory {}: {}", path, e))),
    }
}

/// Create a directory synchronously.
/// fs.mkdirSync(path, options?)
fn mkdir_sync(_ctx: Ctx<'_>, path: String, _options: Opt<Value<'_>>) -> Result<()> {
    // On Android SAF, directories are created implicitly when creating files
    // For now, just verify the parent exists
    if !saf::document_exists(&path) {
        // Directory doesn't exist - that's OK for mkdir
        // SAF will create it when we create a file inside
        Ok(())
    } else {
        Ok(())
    }
}

/// Delete a file synchronously.
/// fs.unlinkSync(path)
fn unlink_sync(ctx: Ctx<'_>, path: String) -> Result<()> {
    match saf::delete_document(&path) {
        Ok(()) => Ok(()),
        Err(e) => Err(make_fs_error(&ctx, "ENOENT", &format!("Failed to delete {}: {}", path, e))),
    }
}

/// Remove a directory synchronously.
/// fs.rmdirSync(path)
fn rmdir_sync(ctx: Ctx<'_>, path: String) -> Result<()> {
    // On Android SAF, we can delete directories the same way as files
    match saf::delete_document(&path) {
        Ok(()) => Ok(()),
        Err(e) => Err(make_fs_error(&ctx, "ENOENT", &format!("Failed to remove directory {}: {}", path, e))),
    }
}

/// Rename/move a file synchronously.
/// fs.renameSync(oldPath, newPath)
fn rename_sync(ctx: Ctx<'_>, old_path: String, new_path: String) -> Result<()> {
    // SAF doesn't have a direct rename operation
    // We need to copy then delete
    match saf::read_document_bytes(&old_path) {
        Ok(content) => {
            match saf::write_document_bytes(&new_path, &content) {
                Ok(()) => {
                    let _ = saf::delete_document(&old_path);
                    Ok(())
                }
                Err(e) => Err(make_fs_error(&ctx, "EACCES", &format!("Failed to write {}: {}", new_path, e))),
            }
        }
        Err(e) => Err(make_fs_error(&ctx, "ENOENT", &format!("Failed to read {}: {}", old_path, e))),
    }
}

/// Copy a file synchronously.
/// fs.copyFileSync(src, dest)
fn copy_file_sync(ctx: Ctx<'_>, src: String, dest: String) -> Result<()> {
    match saf::read_document_bytes(&src) {
        Ok(content) => {
            match saf::write_document_bytes(&dest, &content) {
                Ok(()) => Ok(()),
                Err(e) => Err(make_fs_error(&ctx, "EACCES", &format!("Failed to write {}: {}", dest, e))),
            }
        }
        Err(e) => Err(make_fs_error(&ctx, "ENOENT", &format!("Failed to read {}: {}", src, e))),
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// Extract encoding from options parameter.
fn get_encoding_option(_ctx: &Ctx<'_>, options: Option<Value<'_>>) -> Result<Option<String>> {
    match options {
        Some(opt) => {
            if opt.is_string() {
                // Options is just the encoding string
                Ok(Some(opt.as_string().unwrap().to_string()?))
            } else if let Some(obj) = opt.as_object() {
                // Options is an object with encoding property
                if let Ok(enc) = obj.get::<_, String>("encoding") {
                    Ok(Some(enc))
                } else {
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        }
        None => Ok(None),
    }
}

/// Create a Stats-like object for statSync.
fn create_stats_object<'a>(ctx: &Ctx<'a>, is_directory: bool) -> Result<Object<'a>> {
    let stats = Object::new(ctx.clone())?;

    // Basic stats properties
    stats.set("size", 0i64)?;  // We don't have size info from SAF easily
    stats.set("mode", if is_directory { 0o755i32 } else { 0o644i32 })?;

    // Methods
    let is_dir = is_directory;
    stats.set("isDirectory", Function::new(ctx.clone(), move || is_dir))?;
    stats.set("isFile", Function::new(ctx.clone(), move || !is_dir))?;
    stats.set("isSymbolicLink", Function::new(ctx.clone(), || false))?;
    stats.set("isBlockDevice", Function::new(ctx.clone(), || false))?;
    stats.set("isCharacterDevice", Function::new(ctx.clone(), || false))?;
    stats.set("isFIFO", Function::new(ctx.clone(), || false))?;
    stats.set("isSocket", Function::new(ctx.clone(), || false))?;

    Ok(stats)
}

/// Create a Node.js-style error for fs operations.
/// Returns an Error that can be used with Err() directly.
fn make_fs_error(_ctx: &Ctx<'_>, code: &str, message: &str) -> rquickjs::Error {
    // Use FromJs error with a descriptive message
    rquickjs::Error::FromJs {
        from: "fs",
        to: "operation",
        message: Some(format!("{}: {}", code, message)),
    }
}
