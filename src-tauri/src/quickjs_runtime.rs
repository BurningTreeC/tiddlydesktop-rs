//! QuickJS runtime for running TiddlyWiki on Android without Node.js
//!
//! This module provides:
//! - A QuickJS JavaScript runtime
//! - Node.js-compatible fs, path, and process polyfills
//! - Functions to run TiddlyWiki commands (init, render)

use rquickjs::{Context, Runtime, Function, Object, Value, Ctx, Result as JsResult, IntoJs, Error as JsError};
use rquickjs::function::Rest;
use std::path::{Path, PathBuf};

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
            "#, wiki_path.display().to_string().replace('\\', "\\\\").replace('"', "\\\""), edition);

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
                wiki_path.display().to_string().replace('\\', "\\\\").replace('"', "\\\""),
                output_path.display().to_string().replace('\\', "\\\\").replace('"', "\\\""),
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

        // console.log - simplified version
        let log_fn = Function::new(ctx.clone(), |_ctx: Ctx, args: Rest<String>| {
            let output: String = args.0.join(" ");
            println!("[TW] {}", output);
            Ok::<_, rquickjs::Error>(())
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
        let platform = "android";
        #[cfg(target_os = "linux")]
        let platform = "linux";
        #[cfg(target_os = "macos")]
        let platform = "darwin";
        #[cfg(target_os = "windows")]
        let platform = "win32";
        #[cfg(not(any(target_os = "android", target_os = "linux", target_os = "macos", target_os = "windows")))]
        let platform = "unknown";

        process.set("platform", platform)
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
        // Create modules upfront and store in globals to avoid closure lifetime issues
        // Then use JavaScript to implement require() dispatch

        // Create fs module
        let fs = Object::new(ctx.clone()).map_err(|e| format!("Failed to create fs: {}", e))?;
        let read_fn = Function::new(ctx.clone(), |_ctx: Ctx, path: String, _opts: Option<String>| -> JsResult<String> {
            std::fs::read_to_string(&path).map_err(|_| JsError::Exception)
        }).map_err(|e| format!("Failed to create readFileSync: {}", e))?;
        fs.set("readFileSync", read_fn).map_err(|e| format!("{}", e))?;

        let write_fn = Function::new(ctx.clone(), |_ctx: Ctx, path: String, data: String, _opts: Option<String>| -> JsResult<()> {
            if let Some(parent) = Path::new(&path).parent() { let _ = std::fs::create_dir_all(parent); }
            std::fs::write(&path, data).map_err(|_| JsError::Exception)
        }).map_err(|e| format!("{}", e))?;
        fs.set("writeFileSync", write_fn).map_err(|e| format!("{}", e))?;

        let exists_fn = Function::new(ctx.clone(), |_ctx: Ctx, path: String| -> JsResult<bool> {
            Ok(Path::new(&path).exists())
        }).map_err(|e| format!("{}", e))?;
        fs.set("existsSync", exists_fn).map_err(|e| format!("{}", e))?;

        let readdir_fn = Function::new(ctx.clone(), |_ctx: Ctx, path: String| -> JsResult<Vec<String>> {
            std::fs::read_dir(&path)
                .map(|entries| entries.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect())
                .map_err(|_| JsError::Exception)
        }).map_err(|e| format!("{}", e))?;
        fs.set("readdirSync", readdir_fn).map_err(|e| format!("{}", e))?;

        let is_dir_fn = Function::new(ctx.clone(), |_ctx: Ctx, path: String| -> JsResult<bool> {
            Ok(Path::new(&path).is_dir())
        }).map_err(|e| format!("{}", e))?;
        fs.set("_isDirectory", is_dir_fn).map_err(|e| format!("{}", e))?;

        let mkdir_fn = Function::new(ctx.clone(), |_ctx: Ctx, path: String, _opts: Option<String>| -> JsResult<()> {
            std::fs::create_dir_all(&path).map_err(|_| JsError::Exception)
        }).map_err(|e| format!("{}", e))?;
        fs.set("mkdirSync", mkdir_fn).map_err(|e| format!("{}", e))?;

        let unlink_fn = Function::new(ctx.clone(), |_ctx: Ctx, path: String| -> JsResult<()> {
            std::fs::remove_file(&path).map_err(|_| JsError::Exception)
        }).map_err(|e| format!("{}", e))?;
        fs.set("unlinkSync", unlink_fn).map_err(|e| format!("{}", e))?;

        let copy_fn = Function::new(ctx.clone(), |_ctx: Ctx, src: String, dest: String| -> JsResult<()> {
            std::fs::copy(&src, &dest).map(|_| ()).map_err(|_| JsError::Exception)
        }).map_err(|e| format!("{}", e))?;
        fs.set("copyFileSync", copy_fn).map_err(|e| format!("{}", e))?;

        let rmdir_fn = Function::new(ctx.clone(), |_ctx: Ctx, path: String| -> JsResult<()> {
            std::fs::remove_dir_all(&path).map_err(|_| JsError::Exception)
        }).map_err(|e| format!("{}", e))?;
        fs.set("rmdirSync", rmdir_fn).map_err(|e| format!("{}", e))?;

        let rename_fn = Function::new(ctx.clone(), |_ctx: Ctx, old_path: String, new_path: String| -> JsResult<()> {
            std::fs::rename(&old_path, &new_path).map_err(|_| JsError::Exception)
        }).map_err(|e| format!("{}", e))?;
        fs.set("renameSync", rename_fn).map_err(|e| format!("{}", e))?;

        globals.set("__fs", fs).map_err(|e| format!("{}", e))?;

        // Add statSync and lstatSync via JavaScript since they need to return objects with methods
        let stat_code = r#"
            __fs.statSync = function(path) {
                if (!__fs.existsSync(path)) {
                    throw new Error("ENOENT: no such file or directory, stat '" + path + "'");
                }
                var isDir = __fs._isDirectory(path);
                return {
                    isDirectory: function() { return isDir; },
                    isFile: function() { return !isDir; },
                    isSymbolicLink: function() { return false; },
                    size: 0,
                    mtime: new Date(),
                    atime: new Date(),
                    ctime: new Date()
                };
            };
            __fs.lstatSync = __fs.statSync;
            __fs.realpathSync = function(path) { return path; };
            __fs.readlinkSync = function(path) { return path; };
            __fs.watch = function() { return { close: function() {} }; };
            __fs.watchFile = function() {};
            __fs.unwatchFile = function() {};
        "#;
        ctx.eval::<(), _>(stat_code.as_bytes())
            .map_err(|e| format!("Failed to add statSync: {}", e))?;

        // Create path module
        let path_mod = Object::new(ctx.clone()).map_err(|e| format!("{}", e))?;
        let join_fn = Function::new(ctx.clone(), |_ctx: Ctx, args: Rest<String>| -> JsResult<String> {
            let mut result = PathBuf::new();
            for arg in args.0 {
                if arg.starts_with('/') { result = PathBuf::from(&arg); } else { result.push(&arg); }
            }
            Ok(result.to_string_lossy().to_string())
        }).map_err(|e| format!("{}", e))?;
        path_mod.set("join", join_fn).map_err(|e| format!("{}", e))?;

        let resolve_fn = Function::new(ctx.clone(), |_ctx: Ctx, args: Rest<String>| -> JsResult<String> {
            let mut result = std::env::current_dir().unwrap_or_default();
            for arg in args.0 {
                if arg.starts_with('/') { result = PathBuf::from(&arg); } else { result.push(&arg); }
            }
            Ok(result.to_string_lossy().to_string())
        }).map_err(|e| format!("{}", e))?;
        path_mod.set("resolve", resolve_fn).map_err(|e| format!("{}", e))?;

        let dirname_fn = Function::new(ctx.clone(), |_ctx: Ctx, p: String| -> JsResult<String> {
            Ok(Path::new(&p).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| ".".to_string()))
        }).map_err(|e| format!("{}", e))?;
        path_mod.set("dirname", dirname_fn).map_err(|e| format!("{}", e))?;

        let basename_fn = Function::new(ctx.clone(), |_ctx: Ctx, p: String, ext: Option<String>| -> JsResult<String> {
            let name = Path::new(&p).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if let Some(ext) = ext { if name.ends_with(&ext) { return Ok(name[..name.len() - ext.len()].to_string()); } }
            Ok(name)
        }).map_err(|e| format!("{}", e))?;
        path_mod.set("basename", basename_fn).map_err(|e| format!("{}", e))?;

        let extname_fn = Function::new(ctx.clone(), |_ctx: Ctx, p: String| -> JsResult<String> {
            Ok(Path::new(&p).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default())
        }).map_err(|e| format!("{}", e))?;
        path_mod.set("extname", extname_fn).map_err(|e| format!("{}", e))?;

        path_mod.set("sep", "/").map_err(|e| format!("{}", e))?;
        path_mod.set("delimiter", ":").map_err(|e| format!("{}", e))?;

        let is_abs_fn = Function::new(ctx.clone(), |_ctx: Ctx, p: String| -> JsResult<bool> {
            Ok(Path::new(&p).is_absolute())
        }).map_err(|e| format!("{}", e))?;
        path_mod.set("isAbsolute", is_abs_fn).map_err(|e| format!("{}", e))?;

        let normalize_fn = Function::new(ctx.clone(), |_ctx: Ctx, p: String| -> JsResult<String> {
            use std::path::Component;
            let path = Path::new(&p);
            let mut components = Vec::new();
            for component in path.components() {
                match component {
                    Component::ParentDir => { components.pop(); }
                    Component::CurDir => {}
                    Component::Normal(c) => { components.push(c.to_string_lossy().to_string()); }
                    Component::RootDir => { components.clear(); components.push(String::new()); }
                    Component::Prefix(_) => {}
                }
            }
            if components.is_empty() { return Ok(".".to_string()); }
            if components.len() == 1 && components[0].is_empty() { return Ok("/".to_string()); }
            Ok(components.join("/"))
        }).map_err(|e| format!("{}", e))?;
        path_mod.set("normalize", normalize_fn).map_err(|e| format!("{}", e))?;

        let relative_fn = Function::new(ctx.clone(), |_ctx: Ctx, from: String, to: String| -> JsResult<String> {
            // Simplified relative path - just return the 'to' path if they share no common prefix
            let from_parts: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
            let to_parts: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
            let mut common = 0;
            for (a, b) in from_parts.iter().zip(to_parts.iter()) {
                if a == b { common += 1; } else { break; }
            }
            let up = from_parts.len() - common;
            let mut result = vec![".."; up];
            result.extend(to_parts[common..].iter().cloned());
            if result.is_empty() { return Ok(".".to_string()); }
            Ok(result.join("/"))
        }).map_err(|e| format!("{}", e))?;
        path_mod.set("relative", relative_fn).map_err(|e| format!("{}", e))?;

        globals.set("__path", path_mod).map_err(|e| format!("{}", e))?;

        // Create os module
        let os = Object::new(ctx.clone()).map_err(|e| format!("{}", e))?;
        let platform_fn = Function::new(ctx.clone(), |_ctx: Ctx| -> JsResult<String> {
            #[cfg(target_os = "android")]
            return Ok("android".to_string());
            #[cfg(not(target_os = "android"))]
            return Ok("linux".to_string());
        }).map_err(|e| format!("{}", e))?;
        os.set("platform", platform_fn).map_err(|e| format!("{}", e))?;
        os.set("EOL", "\n").map_err(|e| format!("{}", e))?;

        let homedir_fn = Function::new(ctx.clone(), |_ctx: Ctx| -> JsResult<String> {
            Ok(std::env::var("HOME").unwrap_or_else(|_| "/data/data".to_string()))
        }).map_err(|e| format!("{}", e))?;
        os.set("homedir", homedir_fn).map_err(|e| format!("{}", e))?;

        let tmpdir_fn = Function::new(ctx.clone(), |_ctx: Ctx| -> JsResult<String> {
            Ok(std::env::temp_dir().to_string_lossy().to_string())
        }).map_err(|e| format!("{}", e))?;
        os.set("tmpdir", tmpdir_fn).map_err(|e| format!("{}", e))?;

        let hostname_fn = Function::new(ctx.clone(), |_ctx: Ctx| -> JsResult<String> {
            Ok("localhost".to_string())
        }).map_err(|e| format!("{}", e))?;
        os.set("hostname", hostname_fn).map_err(|e| format!("{}", e))?;

        let type_fn = Function::new(ctx.clone(), |_ctx: Ctx| -> JsResult<String> {
            Ok("Linux".to_string())
        }).map_err(|e| format!("{}", e))?;
        os.set("type", type_fn).map_err(|e| format!("{}", e))?;

        let arch_fn = Function::new(ctx.clone(), |_ctx: Ctx| -> JsResult<String> {
            #[cfg(target_arch = "aarch64")]
            return Ok("arm64".to_string());
            #[cfg(target_arch = "x86_64")]
            return Ok("x64".to_string());
            #[cfg(target_arch = "arm")]
            return Ok("arm".to_string());
            #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64", target_arch = "arm")))]
            return Ok("unknown".to_string());
        }).map_err(|e| format!("{}", e))?;
        os.set("arch", arch_fn).map_err(|e| format!("{}", e))?;

        globals.set("__os", os).map_err(|e| format!("{}", e))?;

        // Create stub modules
        let stub = Object::new(ctx.clone()).map_err(|e| format!("{}", e))?;
        globals.set("__stub", stub).map_err(|e| format!("{}", e))?;

        // Store tiddlywiki path for file module loading
        globals.set("__twpath", self.tiddlywiki_path.to_string_lossy().to_string()).map_err(|e| format!("{}", e))?;

        // Create require function in JavaScript that dispatches to our modules
        let require_code = r#"
            var __modules = {
                fs: __fs,
                path: __path,
                os: __os,
                crypto: __stub,
                zlib: __stub,
                http: __stub,
                https: __stub,
                url: __stub,
                util: __stub,
                events: __stub,
                stream: __stub
            };
            var __moduleCache = {};
            var __currentDir = __twpath;

            function __resolvePath(name, fromDir) {
                if (name.startsWith('/')) {
                    return name;
                }
                var base = fromDir || __currentDir;
                var parts = base.split('/').filter(function(p) { return p !== ''; });
                var nameParts = name.split('/');

                for (var i = 0; i < nameParts.length; i++) {
                    var part = nameParts[i];
                    if (part === '..') {
                        parts.pop();
                    } else if (part !== '.') {
                        parts.push(part);
                    }
                }
                return '/' + parts.join('/');
            }

            function __tryExtensions(basePath) {
                // Try exact path first
                if (__fs.existsSync(basePath)) {
                    if (__fs._isDirectory(basePath)) {
                        // Try index.js in directory
                        var indexPath = basePath + '/index.js';
                        if (__fs.existsSync(indexPath)) return indexPath;
                        // Try package.json main
                        var pkgPath = basePath + '/package.json';
                        if (__fs.existsSync(pkgPath)) {
                            try {
                                var pkg = JSON.parse(__fs.readFileSync(pkgPath));
                                if (pkg.main) {
                                    var mainPath = __resolvePath(pkg.main, basePath);
                                    return __tryExtensions(mainPath);
                                }
                            } catch(e) {}
                        }
                        return indexPath; // fallback
                    }
                    return basePath;
                }
                // Try with .js extension
                if (__fs.existsSync(basePath + '.js')) return basePath + '.js';
                // Try with .json extension
                if (__fs.existsSync(basePath + '.json')) return basePath + '.json';
                return null;
            }

            function require(name) {
                // Built-in modules
                if (__modules[name]) return __modules[name];

                // Resolve path
                var resolved;
                if (name.startsWith('./') || name.startsWith('../') || name.startsWith('/')) {
                    resolved = __resolvePath(name, __currentDir);
                } else {
                    // Node modules - try in tiddlywiki path
                    resolved = __twpath + '/node_modules/' + name;
                    if (!__fs.existsSync(resolved)) {
                        resolved = __twpath + '/' + name;
                    }
                }

                var fullPath = __tryExtensions(resolved);
                if (!fullPath) {
                    throw new Error("Cannot find module '" + name + "' (resolved to " + resolved + ")");
                }

                // Check cache
                if (__moduleCache[fullPath]) return __moduleCache[fullPath].exports;

                // Load module
                var code = __fs.readFileSync(fullPath);

                // Handle JSON files
                if (fullPath.endsWith('.json')) {
                    var jsonExports = JSON.parse(code);
                    __moduleCache[fullPath] = { exports: jsonExports };
                    return jsonExports;
                }

                // Create module object
                var module = { exports: {} };
                var exports = module.exports;
                __moduleCache[fullPath] = module;

                // Save and set current directory for nested requires
                var prevDir = __currentDir;
                var lastSlash = fullPath.lastIndexOf('/');
                __currentDir = lastSlash > 0 ? fullPath.substring(0, lastSlash) : '/';

                // Wrap and execute
                var dirname = __currentDir;
                var filename = fullPath;

                try {
                    var wrapped = '(function(exports, require, module, __filename, __dirname) { ' + code + '\n});';
                    var fn = eval(wrapped);
                    fn(exports, require, module, filename, dirname);
                } finally {
                    __currentDir = prevDir;
                }

                return module.exports;
            }
        "#;
        ctx.eval::<(), _>(require_code.as_bytes())
            .map_err(|e| format!("Failed to create require: {}", e))?;

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
