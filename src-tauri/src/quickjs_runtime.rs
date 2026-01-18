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
        let tw_path = self.tiddlywiki_path.clone();

        // Create the require function
        // Note: We implement require by evaluating JavaScript that creates the module objects
        // This avoids complex Rust lifetime issues with rquickjs closures
        let require_fn = Function::new(ctx.clone(), move |ctx: Ctx<'_>, module_name: String| -> JsResult<Value<'_>> {
            // For built-in modules, we create them inline to avoid lifetime issues
            match module_name.as_str() {
                "fs" | "path" | "os" | "crypto" | "zlib" | "http" | "https" | "url" | "util" | "events" | "stream" => {
                    // Create module object inline
                    let module = Object::new(ctx.clone())?;

                    if module_name == "fs" {
                        // Add fs methods
                        let read_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, path: String, _opts: Option<String>| -> JsResult<String> {
                            std::fs::read_to_string(&path).map_err(|e| JsError::Exception)
                        })?;
                        module.set("readFileSync", read_fn)?;

                        let write_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, path: String, data: String, _opts: Option<String>| -> JsResult<()> {
                            if let Some(parent) = Path::new(&path).parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            std::fs::write(&path, data).map_err(|_| JsError::Exception)
                        })?;
                        module.set("writeFileSync", write_fn)?;

                        let exists_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, path: String| -> JsResult<bool> {
                            Ok(Path::new(&path).exists())
                        })?;
                        module.set("existsSync", exists_fn)?;

                        let readdir_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, path: String| -> JsResult<Vec<String>> {
                            std::fs::read_dir(&path)
                                .map(|entries| entries.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect())
                                .map_err(|_| JsError::Exception)
                        })?;
                        module.set("readdirSync", readdir_fn)?;

                        let is_dir_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, path: String| -> JsResult<bool> {
                            Ok(Path::new(&path).is_dir())
                        })?;
                        module.set("_isDirectory", is_dir_fn)?;

                        let mkdir_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, path: String, _opts: Option<String>| -> JsResult<()> {
                            std::fs::create_dir_all(&path).map_err(|_| JsError::Exception)
                        })?;
                        module.set("mkdirSync", mkdir_fn)?;

                        let unlink_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, path: String| -> JsResult<()> {
                            std::fs::remove_file(&path).map_err(|_| JsError::Exception)
                        })?;
                        module.set("unlinkSync", unlink_fn)?;

                        let copy_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, src: String, dest: String| -> JsResult<()> {
                            std::fs::copy(&src, &dest).map(|_| ()).map_err(|_| JsError::Exception)
                        })?;
                        module.set("copyFileSync", copy_fn)?;
                    } else if module_name == "path" {
                        let join_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, args: Rest<String>| -> JsResult<String> {
                            let mut result = PathBuf::new();
                            for arg in args.0 {
                                if arg.starts_with('/') { result = PathBuf::from(&arg); } else { result.push(&arg); }
                            }
                            Ok(result.to_string_lossy().to_string())
                        })?;
                        module.set("join", join_fn)?;

                        let resolve_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, args: Rest<String>| -> JsResult<String> {
                            let mut result = std::env::current_dir().unwrap_or_default();
                            for arg in args.0 {
                                if arg.starts_with('/') { result = PathBuf::from(&arg); } else { result.push(&arg); }
                            }
                            Ok(result.to_string_lossy().to_string())
                        })?;
                        module.set("resolve", resolve_fn)?;

                        let dirname_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, p: String| -> JsResult<String> {
                            Ok(Path::new(&p).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| ".".to_string()))
                        })?;
                        module.set("dirname", dirname_fn)?;

                        let basename_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, p: String, ext: Option<String>| -> JsResult<String> {
                            let name = Path::new(&p).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                            if let Some(ext) = ext { if name.ends_with(&ext) { return Ok(name[..name.len() - ext.len()].to_string()); } }
                            Ok(name)
                        })?;
                        module.set("basename", basename_fn)?;

                        let extname_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, p: String| -> JsResult<String> {
                            Ok(Path::new(&p).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default())
                        })?;
                        module.set("extname", extname_fn)?;

                        module.set("sep", "/")?;

                        let is_abs_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>, p: String| -> JsResult<bool> {
                            Ok(Path::new(&p).is_absolute())
                        })?;
                        module.set("isAbsolute", is_abs_fn)?;
                    } else if module_name == "os" {
                        let platform_fn = Function::new(ctx.clone(), |_ctx: Ctx<'_>| -> JsResult<String> {
                            #[cfg(target_os = "android")]
                            return Ok("android".to_string());
                            #[cfg(not(target_os = "android"))]
                            return Ok("linux".to_string());
                        })?;
                        module.set("platform", platform_fn)?;
                        module.set("EOL", "\n")?;
                    }
                    // Other modules return empty stub objects

                    module.into_js(&ctx)
                }
                _ => {
                    // Try to load as a file module
                    let module_path = if module_name.starts_with("./") || module_name.starts_with("../") || module_name.starts_with('/') {
                        PathBuf::from(&module_name)
                    } else {
                        tw_path.join(&module_name)
                    };

                    let mut mp = module_path.clone();
                    if !mp.exists() && mp.extension().is_none() { mp.set_extension("js"); }
                    if mp.is_dir() { mp.push("index.js"); }

                    if !mp.exists() {
                        return Err(JsError::Exception);
                    }

                    let code = std::fs::read_to_string(&mp).map_err(|_| JsError::Exception)?;
                    let wrapped = format!(r#"(function(exports, require, module, __filename, __dirname) {{ {} return module.exports; }})(
                        {{}}, require, {{ exports: {{}} }}, "{}", "{}")"#,
                        code,
                        mp.display().to_string().replace('\\', "\\\\").replace('"', "\\\""),
                        mp.parent().unwrap_or(Path::new(".")).display().to_string().replace('\\', "\\\\").replace('"', "\\\"")
                    );
                    ctx.eval(wrapped.as_bytes())
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
