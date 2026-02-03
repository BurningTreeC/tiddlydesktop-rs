//! TiddlyWiki runtime implementation using QuickJS.
//!
//! This module provides the main entry point for running TiddlyWiki
//! on Android using QuickJS instead of Node.js.

use rquickjs::{Context, Runtime, Object, Function, Value};
use rquickjs::function::Rest;
use super::{fs_module, path_module};

/// TiddlyWiki runtime state.
pub struct TiddlyWikiRuntime {
    runtime: Runtime,
    context: Context,
}

impl TiddlyWikiRuntime {
    /// Create a new TiddlyWiki runtime.
    pub fn new() -> std::result::Result<Self, String> {
        // Create QuickJS runtime with reasonable memory limits
        let runtime = Runtime::new().map_err(|e| format!("Failed to create QuickJS runtime: {:?}", e))?;

        // Set memory limit (64MB should be plenty for TiddlyWiki)
        runtime.set_memory_limit(64 * 1024 * 1024);

        // Create context
        let context = Context::full(&runtime)
            .map_err(|e| format!("Failed to create QuickJS context: {:?}", e))?;

        let tw_runtime = Self { runtime, context };

        // Initialize the runtime with our custom modules
        tw_runtime.init_modules()?;

        Ok(tw_runtime)
    }

    /// Initialize custom Node.js-compatible modules.
    fn init_modules(&self) -> std::result::Result<(), String> {
        self.context.with(|ctx| {
            // Register path module
            path_module::register(&ctx)
                .map_err(|e| format!("Failed to register path module: {:?}", e))?;

            // Register fs module
            fs_module::register(&ctx)
                .map_err(|e| format!("Failed to register fs module: {:?}", e))?;

            // Set up the require function that returns our modules
            self.setup_require(&ctx)?;

            // Set up console for debugging
            self.setup_console(&ctx)?;

            // Set up process object (minimal)
            self.setup_process(&ctx)?;

            Ok(())
        })
    }

    /// Set up a minimal require() function.
    fn setup_require(&self, ctx: &rquickjs::Ctx<'_>) -> std::result::Result<(), String> {
        // Set up require as a JavaScript function that looks up modules from globals
        // This avoids Rust closure lifetime issues with rquickjs
        let require_code = r#"
            (function() {
                return function require(moduleName) {
                    if (moduleName === 'fs') return globalThis.__fs_module;
                    if (moduleName === 'path') return globalThis.__path_module;
                    if (moduleName === 'vm') {
                        return {
                            runInThisContext: function(code) {
                                return eval(code);
                            }
                        };
                    }
                    console.warn('[QuickJS] Warning: Unknown module requested:', moduleName);
                    return {};
                };
            })()
        "#;

        let require_fn: Value = ctx.eval(require_code)
            .map_err(|e| format!("Failed to create require function: {:?}", e))?;

        ctx.globals().set("require", require_fn)
            .map_err(|e| format!("Failed to set require: {:?}", e))?;

        Ok(())
    }

    /// Set up console for debugging output.
    fn setup_console(&self, ctx: &rquickjs::Ctx<'_>) -> std::result::Result<(), String> {
        let globals = ctx.globals();
        let console = Object::new(ctx.clone())
            .map_err(|e| format!("Failed to create console object: {:?}", e))?;

        // console.log
        console.set("log", Function::new(ctx.clone(), |args: Rest<Value<'_>>| {
            let output: Vec<String> = args.0.iter()
                .map(|v| format!("{:?}", v))
                .collect();
            eprintln!("[TiddlyWiki] {}", output.join(" "));
        })).map_err(|e| format!("Failed to set console.log: {:?}", e))?;

        // console.warn
        console.set("warn", Function::new(ctx.clone(), |args: Rest<Value<'_>>| {
            let output: Vec<String> = args.0.iter()
                .map(|v| format!("{:?}", v))
                .collect();
            eprintln!("[TiddlyWiki WARN] {}", output.join(" "));
        })).map_err(|e| format!("Failed to set console.warn: {:?}", e))?;

        // console.error
        console.set("error", Function::new(ctx.clone(), |args: Rest<Value<'_>>| {
            let output: Vec<String> = args.0.iter()
                .map(|v| format!("{:?}", v))
                .collect();
            eprintln!("[TiddlyWiki ERROR] {}", output.join(" "));
        })).map_err(|e| format!("Failed to set console.error: {:?}", e))?;

        globals.set("console", console)
            .map_err(|e| format!("Failed to set console: {:?}", e))?;

        Ok(())
    }

    /// Set up minimal process object.
    fn setup_process(&self, ctx: &rquickjs::Ctx<'_>) -> std::result::Result<(), String> {
        let globals = ctx.globals();
        let process = Object::new(ctx.clone())
            .map_err(|e| format!("Failed to create process object: {:?}", e))?;

        // process.platform
        process.set("platform", "android")
            .map_err(|e| format!("Failed to set process.platform: {:?}", e))?;

        // process.env (empty object)
        let env = Object::new(ctx.clone())
            .map_err(|e| format!("Failed to create process.env: {:?}", e))?;
        process.set("env", env)
            .map_err(|e| format!("Failed to set process.env: {:?}", e))?;

        // process.argv (minimal)
        let argv: Vec<String> = vec!["tiddlydesktop".to_string()];
        process.set("argv", argv)
            .map_err(|e| format!("Failed to set process.argv: {:?}", e))?;

        // process.cwd()
        process.set("cwd", Function::new(ctx.clone(), || -> String {
            "/data/data/com.tiddlydesktop/files".to_string()
        })).map_err(|e| format!("Failed to set process.cwd: {:?}", e))?;

        globals.set("process", process)
            .map_err(|e| format!("Failed to set process: {:?}", e))?;

        Ok(())
    }

    /// Load and execute TiddlyWiki boot.js.
    pub fn load_boot(&self, boot_js_content: &str) -> std::result::Result<(), String> {
        self.context.with(|ctx| {
            ctx.eval::<(), _>(boot_js_content)
                .map_err(|e| format!("Failed to execute boot.js: {:?}", e))
        })
    }

    /// Load a wiki from a directory (content:// URI on Android).
    pub fn load_wiki(&self, wiki_path: &str) -> std::result::Result<(), String> {
        self.context.with(|ctx| {
            // Set the wiki path in the global scope
            let globals = ctx.globals();

            // Create $tw.boot.wikiPath
            let tw: Object = globals.get("$tw")
                .map_err(|e| format!("$tw not found: {:?}", e))?;

            let boot: Object = tw.get("boot")
                .map_err(|e| format!("$tw.boot not found: {:?}", e))?;

            boot.set("wikiPath", wiki_path)
                .map_err(|e| format!("Failed to set wikiPath: {:?}", e))?;

            // Call $tw.boot.boot() to initialize the wiki
            let boot_fn: Function = boot.get("boot")
                .map_err(|e| format!("$tw.boot.boot not found: {:?}", e))?;

            boot_fn.call::<_, ()>(())
                .map_err(|e| format!("Failed to call $tw.boot.boot(): {:?}", e))?;

            Ok(())
        })
    }

    /// Render the wiki to HTML.
    pub fn render_to_html(&self) -> std::result::Result<String, String> {
        self.context.with(|ctx| {
            // Call $tw.wiki.renderTiddler("text/html", "$:/core/save/all")
            let result: String = ctx.eval(r#"
                $tw.wiki.renderTiddler("text/html", "$:/core/save/all")
            "#).map_err(|e| format!("Failed to render wiki: {:?}", e))?;

            Ok(result)
        })
    }

    /// Execute arbitrary JavaScript in the runtime.
    pub fn eval(&self, code: &str) -> std::result::Result<String, String> {
        self.context.with(|ctx| {
            let result: Value = ctx.eval(code)
                .map_err(|e| format!("Eval error: {:?}", e))?;

            // Convert result to string
            Ok(format!("{:?}", result))
        })
    }

    /// Get a tiddler's text content.
    pub fn get_tiddler_text(&self, title: &str) -> std::result::Result<Option<String>, String> {
        self.context.with(|ctx| {
            let code = format!(r#"
                (function() {{
                    var tiddler = $tw.wiki.getTiddler({});
                    return tiddler ? tiddler.fields.text : null;
                }})()
            "#, serde_json::to_string(title).unwrap_or_else(|_| "\"\"".to_string()));

            let result: Value = ctx.eval(code)
                .map_err(|e| format!("Failed to get tiddler: {:?}", e))?;

            if result.is_null() || result.is_undefined() {
                Ok(None)
            } else if let Some(s) = result.as_string() {
                Ok(Some(s.to_string().map_err(|e| format!("String conversion error: {:?}", e))?))
            } else {
                Ok(None)
            }
        })
    }

    /// Set a tiddler's content.
    pub fn set_tiddler(&self, title: &str, text: &str) -> std::result::Result<(), String> {
        self.context.with(|ctx| {
            let code = format!(r#"
                $tw.wiki.addTiddler({{
                    title: {},
                    text: {}
                }});
            "#,
                serde_json::to_string(title).unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
            );

            ctx.eval::<(), _>(code)
                .map_err(|e| format!("Failed to set tiddler: {:?}", e))
        })
    }

    /// Run pending JavaScript jobs (for async operations).
    pub fn run_pending_jobs(&self) -> std::result::Result<(), String> {
        loop {
            if !self.runtime.is_job_pending() {
                break;
            }
            self.runtime.execute_pending_job()
                .map_err(|e| format!("Job execution error: {:?}", e))?;
        }
        Ok(())
    }
}

impl Default for TiddlyWikiRuntime {
    fn default() -> Self {
        Self::new().expect("Failed to create TiddlyWiki runtime")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_creation() {
        let runtime = TiddlyWikiRuntime::new();
        assert!(runtime.is_ok());
    }

    #[test]
    fn test_basic_eval() {
        let runtime = TiddlyWikiRuntime::new().unwrap();
        let result = runtime.eval("1 + 2");
        assert!(result.is_ok());
    }

    #[test]
    fn test_require_path() {
        let runtime = TiddlyWikiRuntime::new().unwrap();
        let result = runtime.eval("require('path').join('a', 'b', 'c')");
        assert!(result.is_ok());
    }
}
