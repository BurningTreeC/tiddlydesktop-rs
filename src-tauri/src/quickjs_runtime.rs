//! QuickJS runtime for running TiddlyWiki on Android without Node.js
//!
//! This module provides:
//! - A QuickJS JavaScript runtime
//! - Node.js-compatible fs, path, and process polyfills
//! - Functions to run TiddlyWiki commands (init, render, server)

use rquickjs::{Context, Runtime, Function, Object, Value, Ctx, Result as JsResult, IntoJs, FromJs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;

/// TiddlyWiki QuickJS Runtime
pub struct TiddlyWikiRuntime {
    runtime: Runtime,
    tiddlywiki_path: PathBuf,
}

impl TiddlyWikiRuntime {
    /// Create a new TiddlyWiki runtime
    pub fn new(tiddlywiki_path: PathBuf) -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|e| format!("Failed to create QuickJS runtime: {}", e))?;

        Ok(Self {
            runtime,
            tiddlywiki_path,
        })
    }

    /// Initialize a new wiki folder with the specified edition
    pub fn init_wiki(&self, wiki_path: &Path, edition: &str) -> Result<(), String> {
        let context = Context::full(&self.runtime)
            .map_err(|e| format!("Failed to create context: {}", e))?;

        context.with(|ctx| {
            // Set up the global environment
            self.setup_globals(&ctx, wiki_path)?;

            // Load and run TiddlyWiki boot
            self.load_tiddlywiki(&ctx)?;

            // Run the init command
            let code = format!(r#"
                $tw.boot.argv = ["{}", "--init", "{}"];
                $tw.boot.boot();
            "#, wiki_path.display().to_string().replace('\\', "\\\\"), edition);

            ctx.eval::<(), _>(code.as_bytes())
                .map_err(|e| format!("Failed to run init: {}", e))?;

            Ok(())
        })
    }

    /// Render a wiki to a single HTML file
    pub fn render_wiki(&self, wiki_path: &Path, output_path: &Path, output_filename: &str) -> Result<(), String> {
        let context = Context::full(&self.runtime)
            .map_err(|e| format!("Failed to create context: {}", e))?;

        context.with(|ctx| {
            // Set up the global environment
            self.setup_globals(&ctx, wiki_path)?;

            // Load and run TiddlyWiki boot
            self.load_tiddlywiki(&ctx)?;

            // Run the render command
            let code = format!(r#"
                $tw.boot.argv = [
                    "{}",
                    "--output", "{}",
                    "--render", "$:/core/save/all", "{}", "text/plain"
                ];
                $tw.boot.boot();
            "#,
                wiki_path.display().to_string().replace('\\', "\\\\"),
                output_path.display().to_string().replace('\\', "\\\\"),
                output_filename
            );

            ctx.eval::<(), _>(code.as_bytes())
                .map_err(|e| format!("Failed to run render: {}", e))?;

            Ok(())
        })
    }

    /// Set up Node.js-compatible globals (fs, path, process, etc.)
    fn setup_globals<'js>(&self, ctx: &Ctx<'js>, working_dir: &Path) -> Result<(), String> {
        let globals = ctx.globals();

        // Set up console
        self.setup_console(ctx, &globals)?;

        // Set up process
        self.setup_process(ctx, &globals, working_dir)?;

        // Set up require and module system
        self.setup_require(ctx, &globals)?;

        Ok(())
    }

    fn setup_console<'js>(&self, ctx: &Ctx<'js>, globals: &Object<'js>) -> Result<(), String> {
        let console = Object::new(ctx.clone())
            .map_err(|e| format!("Failed to create console object: {}", e))?;

        // console.log
        let log_fn = Function::new(ctx.clone(), |ctx: Ctx, args: rquickjs::Rest<Value>| {
            let parts: Vec<String> = args.0.iter()
                .map(|v| format!("{:?}", v))
                .collect();
            println!("[TW] {}", parts.join(" "));
            Ok(())
        }).map_err(|e| format!("Failed to create console.log: {}", e))?;

        console.set("log", log_fn.clone())
            .map_err(|e| format!("Failed to set console.log: {}", e))?;
        console.set("info", log_fn.clone())
            .map_err(|e| format!("Failed to set console.info: {}", e))?;
        console.set("warn", log_fn.clone())
            .map_err(|e| format!("Failed to set console.warn: {}", e))?;
        console.set("error", log_fn)
            .map_err(|e| format!("Failed to set console.error: {}", e))?;

        globals.set("console", console)
            .map_err(|e| format!("Failed to set console global: {}", e))?;

        Ok(())
    }

    fn setup_process<'js>(&self, ctx: &Ctx<'js>, globals: &Object<'js>, working_dir: &Path) -> Result<(), String> {
        let process = Object::new(ctx.clone())
            .map_err(|e| format!("Failed to create process object: {}", e))?;

        // process.platform
        #[cfg(target_os = "android")]
        process.set("platform", "android")
            .map_err(|e| format!("Failed to set process.platform: {}", e))?;
        #[cfg(target_os = "linux")]
        process.set("platform", "linux")
            .map_err(|e| format!("Failed to set process.platform: {}", e))?;
        #[cfg(target_os = "macos")]
        process.set("platform", "darwin")
            .map_err(|e| format!("Failed to set process.platform: {}", e))?;
        #[cfg(target_os = "windows")]
        process.set("platform", "win32")
            .map_err(|e| format!("Failed to set process.platform: {}", e))?;

        // process.argv (will be set before running commands)
        let argv: Vec<String> = vec!["tiddlywiki".to_string()];
        process.set("argv", argv)
            .map_err(|e| format!("Failed to set process.argv: {}", e))?;

        // process.cwd()
        let cwd = working_dir.to_string_lossy().to_string();
        let cwd_fn = Function::new(ctx.clone(), move |_ctx: Ctx| -> JsResult<String> {
            Ok(cwd.clone())
        }).map_err(|e| format!("Failed to create process.cwd: {}", e))?;
        process.set("cwd", cwd_fn)
            .map_err(|e| format!("Failed to set process.cwd: {}", e))?;

        // process.exit()
        let exit_fn = Function::new(ctx.clone(), |_ctx: Ctx, _code: i32| -> JsResult<()> {
            // Don't actually exit, just return
            Ok(())
        }).map_err(|e| format!("Failed to create process.exit: {}", e))?;
        process.set("exit", exit_fn)
            .map_err(|e| format!("Failed to set process.exit: {}", e))?;

        globals.set("process", process)
            .map_err(|e| format!("Failed to set process global: {}", e))?;

        Ok(())
    }

    fn setup_require<'js>(&self, ctx: &Ctx<'js>, globals: &Object<'js>) -> Result<(), String> {
        let tw_path = self.tiddlywiki_path.clone();

        // Create the require function
        let require_fn = Function::new(ctx.clone(), move |ctx: Ctx, module_name: String| -> JsResult<Value> {
            match module_name.as_str() {
                "fs" => create_fs_module(&ctx),
                "path" => create_path_module(&ctx),
                "os" => create_os_module(&ctx),
                "crypto" => create_crypto_module(&ctx),
                "zlib" => create_zlib_module(&ctx),
                "http" => create_http_module(&ctx),
                "https" => create_http_module(&ctx),
                "url" => create_url_module(&ctx),
                "util" => create_util_module(&ctx),
                "events" => create_events_module(&ctx),
                "stream" => create_stream_module(&ctx),
                _ => {
                    // Try to load as a file module
                    let module_path = if module_name.starts_with("./") || module_name.starts_with("../") || module_name.starts_with('/') {
                        PathBuf::from(&module_name)
                    } else {
                        tw_path.join(&module_name)
                    };

                    load_file_module(&ctx, &module_path)
                }
            }
        }).map_err(|e| format!("Failed to create require: {}", e))?;

        globals.set("require", require_fn)
            .map_err(|e| format!("Failed to set require global: {}", e))?;

        Ok(())
    }

    fn load_tiddlywiki<'js>(&self, ctx: &Ctx<'js>) -> Result<(), String> {
        // Load the TiddlyWiki boot code
        let boot_path = self.tiddlywiki_path.join("boot").join("boot.js");
        let bootprefix_path = self.tiddlywiki_path.join("boot").join("bootprefix.js");

        // Read and execute bootprefix.js
        if bootprefix_path.exists() {
            let bootprefix = std::fs::read_to_string(&bootprefix_path)
                .map_err(|e| format!("Failed to read bootprefix.js: {}", e))?;
            ctx.eval::<(), _>(bootprefix.as_bytes())
                .map_err(|e| format!("Failed to execute bootprefix.js: {}", e))?;
        }

        // Read and execute boot.js
        if boot_path.exists() {
            let boot = std::fs::read_to_string(&boot_path)
                .map_err(|e| format!("Failed to read boot.js: {}", e))?;
            ctx.eval::<(), _>(boot.as_bytes())
                .map_err(|e| format!("Failed to execute boot.js: {}", e))?;
        } else {
            return Err(format!("TiddlyWiki boot.js not found at {:?}", boot_path));
        }

        Ok(())
    }
}

// ============================================================================
// Node.js Module Polyfills
// ============================================================================

fn create_fs_module<'js>(ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
    let fs = Object::new(ctx.clone())?;

    // fs.readFileSync
    let read_file_sync = Function::new(ctx.clone(), |_ctx: Ctx, path: String, options: Option<Value>| -> JsResult<String> {
        let encoding = options
            .and_then(|v| {
                if let Ok(s) = String::from_js(&_ctx, v.clone()) {
                    Some(s)
                } else if let Ok(obj) = Object::from_js(&_ctx, v) {
                    obj.get::<_, String>("encoding").ok()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "utf8".to_string());

        match std::fs::read_to_string(&path) {
            Ok(content) => Ok(content),
            Err(e) => Err(rquickjs::Error::Exception {
                message: format!("ENOENT: no such file or directory, open '{}'", path),
                file: String::new(),
                line: 0,
                stack: String::new(),
            })
        }
    })?;
    fs.set("readFileSync", read_file_sync)?;

    // fs.writeFileSync
    let write_file_sync = Function::new(ctx.clone(), |_ctx: Ctx, path: String, data: String, _options: Option<Value>| -> JsResult<()> {
        // Ensure parent directory exists
        if let Some(parent) = Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, data)
            .map_err(|e| rquickjs::Error::Exception {
                message: format!("Failed to write file '{}': {}", path, e),
                file: String::new(),
                line: 0,
                stack: String::new(),
            })
    })?;
    fs.set("writeFileSync", write_file_sync)?;

    // fs.existsSync
    let exists_sync = Function::new(ctx.clone(), |_ctx: Ctx, path: String| -> JsResult<bool> {
        Ok(Path::new(&path).exists())
    })?;
    fs.set("existsSync", exists_sync)?;

    // fs.readdirSync
    let readdir_sync = Function::new(ctx.clone(), |_ctx: Ctx, path: String| -> JsResult<Vec<String>> {
        match std::fs::read_dir(&path) {
            Ok(entries) => {
                let names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect();
                Ok(names)
            }
            Err(e) => Err(rquickjs::Error::Exception {
                message: format!("ENOENT: no such file or directory, scandir '{}'", path),
                file: String::new(),
                line: 0,
                stack: String::new(),
            })
        }
    })?;
    fs.set("readdirSync", readdir_sync)?;

    // fs.statSync
    let stat_sync = Function::new(ctx.clone(), |ctx: Ctx, path: String| -> JsResult<Object> {
        let stat = Object::new(ctx.clone())?;
        let metadata = std::fs::metadata(&path)
            .map_err(|e| rquickjs::Error::Exception {
                message: format!("ENOENT: no such file or directory, stat '{}'", path),
                file: String::new(),
                line: 0,
                stack: String::new(),
            })?;

        let is_dir = metadata.is_dir();
        let is_file = metadata.is_file();

        let is_directory_fn = Function::new(ctx.clone(), move |_ctx: Ctx| -> JsResult<bool> {
            Ok(is_dir)
        })?;
        stat.set("isDirectory", is_directory_fn)?;

        let is_file_fn = Function::new(ctx.clone(), move |_ctx: Ctx| -> JsResult<bool> {
            Ok(is_file)
        })?;
        stat.set("isFile", is_file_fn)?;

        Ok(stat)
    })?;
    fs.set("statSync", stat_sync)?;

    // fs.mkdirSync
    let mkdir_sync = Function::new(ctx.clone(), |_ctx: Ctx, path: String, options: Option<Value>| -> JsResult<()> {
        let recursive = options
            .and_then(|v| {
                if let Ok(obj) = Object::from_js(&_ctx, v) {
                    obj.get::<_, bool>("recursive").ok()
                } else {
                    None
                }
            })
            .unwrap_or(false);

        let result = if recursive {
            std::fs::create_dir_all(&path)
        } else {
            std::fs::create_dir(&path)
        };

        result.map_err(|e| rquickjs::Error::Exception {
            message: format!("Failed to create directory '{}': {}", path, e),
            file: String::new(),
            line: 0,
            stack: String::new(),
        })
    })?;
    fs.set("mkdirSync", mkdir_sync)?;

    // fs.unlinkSync
    let unlink_sync = Function::new(ctx.clone(), |_ctx: Ctx, path: String| -> JsResult<()> {
        std::fs::remove_file(&path)
            .map_err(|e| rquickjs::Error::Exception {
                message: format!("Failed to remove file '{}': {}", path, e),
                file: String::new(),
                line: 0,
                stack: String::new(),
            })
    })?;
    fs.set("unlinkSync", unlink_sync)?;

    // fs.rmdirSync
    let rmdir_sync = Function::new(ctx.clone(), |_ctx: Ctx, path: String| -> JsResult<()> {
        std::fs::remove_dir(&path)
            .map_err(|e| rquickjs::Error::Exception {
                message: format!("Failed to remove directory '{}': {}", path, e),
                file: String::new(),
                line: 0,
                stack: String::new(),
            })
    })?;
    fs.set("rmdirSync", rmdir_sync)?;

    // fs.copyFileSync
    let copy_file_sync = Function::new(ctx.clone(), |_ctx: Ctx, src: String, dest: String| -> JsResult<()> {
        std::fs::copy(&src, &dest)
            .map(|_| ())
            .map_err(|e| rquickjs::Error::Exception {
                message: format!("Failed to copy '{}' to '{}': {}", src, dest, e),
                file: String::new(),
                line: 0,
                stack: String::new(),
            })
    })?;
    fs.set("copyFileSync", copy_file_sync)?;

    fs.into_js(ctx)
}

fn create_path_module<'js>(ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
    let path = Object::new(ctx.clone())?;

    // path.join
    let join_fn = Function::new(ctx.clone(), |_ctx: Ctx, args: rquickjs::Rest<String>| -> JsResult<String> {
        let mut result = PathBuf::new();
        for arg in args.0 {
            if arg.starts_with('/') {
                result = PathBuf::from(&arg);
            } else {
                result.push(&arg);
            }
        }
        Ok(result.to_string_lossy().to_string())
    })?;
    path.set("join", join_fn)?;

    // path.resolve
    let resolve_fn = Function::new(ctx.clone(), |_ctx: Ctx, args: rquickjs::Rest<String>| -> JsResult<String> {
        let mut result = std::env::current_dir().unwrap_or_default();
        for arg in args.0 {
            if arg.starts_with('/') {
                result = PathBuf::from(&arg);
            } else {
                result.push(&arg);
            }
        }
        Ok(result.to_string_lossy().to_string())
    })?;
    path.set("resolve", resolve_fn)?;

    // path.dirname
    let dirname_fn = Function::new(ctx.clone(), |_ctx: Ctx, p: String| -> JsResult<String> {
        Ok(Path::new(&p)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string()))
    })?;
    path.set("dirname", dirname_fn)?;

    // path.basename
    let basename_fn = Function::new(ctx.clone(), |_ctx: Ctx, p: String, ext: Option<String>| -> JsResult<String> {
        let name = Path::new(&p)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if let Some(ext) = ext {
            if name.ends_with(&ext) {
                return Ok(name[..name.len() - ext.len()].to_string());
            }
        }
        Ok(name)
    })?;
    path.set("basename", basename_fn)?;

    // path.extname
    let extname_fn = Function::new(ctx.clone(), |_ctx: Ctx, p: String| -> JsResult<String> {
        Ok(Path::new(&p)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default())
    })?;
    path.set("extname", extname_fn)?;

    // path.sep
    #[cfg(windows)]
    path.set("sep", "\\")?;
    #[cfg(not(windows))]
    path.set("sep", "/")?;

    // path.normalize
    let normalize_fn = Function::new(ctx.clone(), |_ctx: Ctx, p: String| -> JsResult<String> {
        // Simple normalization - remove redundant separators and resolve . and ..
        let path = PathBuf::from(&p);
        let mut components: Vec<&std::ffi::OsStr> = Vec::new();

        for component in path.components() {
            match component {
                std::path::Component::CurDir => {},
                std::path::Component::ParentDir => { components.pop(); },
                std::path::Component::Normal(c) => components.push(c),
                std::path::Component::RootDir => components.clear(),
                std::path::Component::Prefix(p) => components.push(p.as_os_str()),
            }
        }

        if components.is_empty() {
            Ok(".".to_string())
        } else {
            let result: PathBuf = components.iter().collect();
            Ok(result.to_string_lossy().to_string())
        }
    })?;
    path.set("normalize", normalize_fn)?;

    // path.isAbsolute
    let is_absolute_fn = Function::new(ctx.clone(), |_ctx: Ctx, p: String| -> JsResult<bool> {
        Ok(Path::new(&p).is_absolute())
    })?;
    path.set("isAbsolute", is_absolute_fn)?;

    path.into_js(ctx)
}

fn create_os_module<'js>(ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
    let os = Object::new(ctx.clone())?;

    // os.platform()
    let platform_fn = Function::new(ctx.clone(), |_ctx: Ctx| -> JsResult<String> {
        #[cfg(target_os = "android")]
        return Ok("android".to_string());
        #[cfg(target_os = "linux")]
        return Ok("linux".to_string());
        #[cfg(target_os = "macos")]
        return Ok("darwin".to_string());
        #[cfg(target_os = "windows")]
        return Ok("win32".to_string());
        #[cfg(not(any(target_os = "android", target_os = "linux", target_os = "macos", target_os = "windows")))]
        return Ok("unknown".to_string());
    })?;
    os.set("platform", platform_fn)?;

    // os.EOL
    #[cfg(windows)]
    os.set("EOL", "\r\n")?;
    #[cfg(not(windows))]
    os.set("EOL", "\n")?;

    os.into_js(ctx)
}

fn create_crypto_module<'js>(ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
    let crypto = Object::new(ctx.clone())?;

    // Minimal crypto - just enough for TiddlyWiki
    let random_bytes_fn = Function::new(ctx.clone(), |ctx: Ctx, size: usize| -> JsResult<Object> {
        // Return a simple object that can be converted to hex
        let buffer = Object::new(ctx.clone())?;
        let mut bytes = vec![0u8; size];
        // Simple pseudo-random (not cryptographically secure, but TW just needs uniqueness)
        use std::time::{SystemTime, UNIX_EPOCH};
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = ((seed.wrapping_mul(i as u64 + 1)) % 256) as u8;
        }
        let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();

        let to_string_fn = Function::new(ctx.clone(), move |_ctx: Ctx, _encoding: Option<String>| -> JsResult<String> {
            Ok(hex.clone())
        })?;
        buffer.set("toString", to_string_fn)?;

        Ok(buffer)
    })?;
    crypto.set("randomBytes", random_bytes_fn)?;

    crypto.into_js(ctx)
}

fn create_zlib_module<'js>(ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
    // Stub - TiddlyWiki can work without compression
    let zlib = Object::new(ctx.clone())?;
    zlib.into_js(ctx)
}

fn create_http_module<'js>(ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
    // Stub - we'll handle HTTP server separately in Rust
    let http = Object::new(ctx.clone())?;
    http.into_js(ctx)
}

fn create_url_module<'js>(ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
    let url = Object::new(ctx.clone())?;

    // url.parse - basic implementation
    let parse_fn = Function::new(ctx.clone(), |ctx: Ctx, url_str: String| -> JsResult<Object> {
        let parsed = Object::new(ctx.clone())?;

        // Very basic URL parsing
        let (protocol, rest) = if let Some(idx) = url_str.find("://") {
            (url_str[..idx + 1].to_string(), &url_str[idx + 3..])
        } else {
            (String::new(), url_str.as_str())
        };

        let (host, path) = if let Some(idx) = rest.find('/') {
            (rest[..idx].to_string(), rest[idx..].to_string())
        } else {
            (rest.to_string(), "/".to_string())
        };

        parsed.set("protocol", protocol)?;
        parsed.set("host", host.clone())?;
        parsed.set("hostname", host)?;
        parsed.set("pathname", path)?;
        parsed.set("href", url_str)?;

        Ok(parsed)
    })?;
    url.set("parse", parse_fn)?;

    url.into_js(ctx)
}

fn create_util_module<'js>(ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
    let util = Object::new(ctx.clone())?;

    // util.inherits - used by TiddlyWiki
    let inherits_fn = Function::new(ctx.clone(), |ctx: Ctx, ctor: Function, super_ctor: Function| -> JsResult<()> {
        // Basic prototype chain setup
        if let Ok(super_proto) = super_ctor.get::<_, Object>("prototype") {
            ctor.set("super_", super_ctor)?;
            // This is a simplified version
        }
        Ok(())
    })?;
    util.set("inherits", inherits_fn)?;

    util.into_js(ctx)
}

fn create_events_module<'js>(ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
    let events = Object::new(ctx.clone())?;

    // EventEmitter stub
    let event_emitter = Function::new(ctx.clone(), |_ctx: Ctx| -> JsResult<()> {
        Ok(())
    })?;
    events.set("EventEmitter", event_emitter)?;

    events.into_js(ctx)
}

fn create_stream_module<'js>(ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
    let stream = Object::new(ctx.clone())?;

    // Readable stub
    let readable = Function::new(ctx.clone(), |_ctx: Ctx| -> JsResult<()> {
        Ok(())
    })?;
    stream.set("Readable", readable)?;

    stream.into_js(ctx)
}

fn load_file_module<'js>(ctx: &Ctx<'js>, path: &Path) -> JsResult<Value<'js>> {
    // Try to load a JS file as a module
    let mut module_path = path.to_path_buf();

    // Try with .js extension if not present
    if !module_path.exists() && module_path.extension().is_none() {
        module_path.set_extension("js");
    }

    // Try index.js if it's a directory
    if module_path.is_dir() {
        module_path.push("index.js");
    }

    if !module_path.exists() {
        return Err(rquickjs::Error::Exception {
            message: format!("Cannot find module '{}'", path.display()),
            file: String::new(),
            line: 0,
            stack: String::new(),
        });
    }

    let code = std::fs::read_to_string(&module_path)
        .map_err(|e| rquickjs::Error::Exception {
            message: format!("Failed to read module '{}': {}", module_path.display(), e),
            file: String::new(),
            line: 0,
            stack: String::new(),
        })?;

    // Wrap in CommonJS-style module wrapper
    let wrapped = format!(r#"
        (function(exports, require, module, __filename, __dirname) {{
            {}
            return module.exports;
        }})(
            {{}},
            require,
            {{ exports: {{}} }},
            "{}",
            "{}"
        )
    "#,
        code,
        module_path.display().to_string().replace('\\', "\\\\"),
        module_path.parent().unwrap_or(Path::new(".")).display().to_string().replace('\\', "\\\\")
    );

    ctx.eval(wrapped.as_bytes())
}

// ============================================================================
// High-level API for use from lib.rs
// ============================================================================

/// Initialize a wiki folder using QuickJS (for Android)
pub fn quickjs_init_wiki(tiddlywiki_path: &Path, wiki_path: &Path, edition: &str) -> Result<(), String> {
    let runtime = TiddlyWikiRuntime::new(tiddlywiki_path.to_path_buf())?;
    runtime.init_wiki(wiki_path, edition)
}

/// Render a wiki to HTML using QuickJS (for Android)
pub fn quickjs_render_wiki(
    tiddlywiki_path: &Path,
    wiki_path: &Path,
    output_path: &Path,
    output_filename: &str
) -> Result<(), String> {
    let runtime = TiddlyWikiRuntime::new(tiddlywiki_path.to_path_buf())?;
    runtime.render_wiki(wiki_path, output_path, output_filename)
}
