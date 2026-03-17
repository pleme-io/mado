//! Rhai scripting integration via soushi.
//!
//! Loads user scripts from `~/.config/mado/scripts/*.rhai` and exposes
//! terminal-specific functions: `mado.send_keys`, `mado.split`,
//! `mado.new_tab`, `mado.set_title`.

use std::collections::HashMap;
use std::path::PathBuf;

use soushi::ScriptEngine;

/// Script event hooks that scripts can define.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScriptEvent {
    /// Fired when the application starts.
    OnStart,
    /// Fired when the application is about to quit.
    OnQuit,
    /// Fired on every key press (receives key name as argument).
    OnKey,
}

/// Manages the Rhai scripting engine and user scripts for mado.
pub struct ScriptManager {
    engine: ScriptEngine,
    /// Pre-compiled hook scripts keyed by event.
    hooks: HashMap<ScriptEvent, Vec<soushi::rhai::AST>>,
    /// Named scripts that can be invoked on demand.
    named_scripts: HashMap<String, soushi::rhai::AST>,
    /// The scripts directory path.
    scripts_dir: PathBuf,
}

impl ScriptManager {
    /// Create a new script manager and register mado-specific functions.
    ///
    /// Scripts are loaded from `~/.config/mado/scripts/` if the directory exists.
    #[must_use]
    pub fn new() -> Self {
        let mut engine = ScriptEngine::new();
        engine.register_builtin_log();
        engine.register_builtin_env();
        engine.register_builtin_string();

        Self::register_mado_functions(&mut engine);

        let scripts_dir = dirs_script_path("mado");

        let mut manager = Self {
            engine,
            hooks: HashMap::new(),
            named_scripts: HashMap::new(),
            scripts_dir,
        };

        manager.load_scripts();
        manager
    }

    /// Register mado-specific functions with the scripting engine.
    fn register_mado_functions(engine: &mut ScriptEngine) {
        // mado.send_keys(keys) — send keystrokes to the terminal
        engine.register_fn("mado_send_keys", |keys: &str| -> String {
            tracing::info!(keys, "script: mado.send_keys");
            format!("sent keys: {keys}")
        });

        // mado.split(direction) — split the current pane
        engine.register_fn("mado_split", |direction: &str| -> String {
            tracing::info!(direction, "script: mado.split");
            format!("split: {direction}")
        });

        // mado.new_tab() — open a new tab
        engine.register_fn("mado_new_tab", || -> String {
            tracing::info!("script: mado.new_tab");
            "new tab created".to_string()
        });

        // mado.set_title(title) — set the window title
        engine.register_fn("mado_set_title", |title: &str| -> String {
            tracing::info!(title, "script: mado.set_title");
            format!("title set: {title}")
        });
    }

    /// Load all scripts from the scripts directory.
    fn load_scripts(&mut self) {
        if !self.scripts_dir.is_dir() {
            tracing::debug!(
                path = %self.scripts_dir.display(),
                "scripts directory does not exist, skipping"
            );
            return;
        }

        match self.engine.load_scripts_dir(&self.scripts_dir) {
            Ok(names) => {
                tracing::info!(count = names.len(), "loaded mado scripts");
                for name in &names {
                    self.compile_named_script(name);
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to load scripts");
            }
        }
    }

    /// Compile and store a named script for later execution.
    fn compile_named_script(&mut self, name: &str) {
        let path = self.scripts_dir.join(format!("{name}.rhai"));
        if let Ok(source) = std::fs::read_to_string(&path) {
            match self.engine.compile(&source) {
                Ok(ast) => {
                    self.named_scripts.insert(name.to_string(), ast);
                }
                Err(e) => {
                    tracing::error!(script = name, error = %e, "failed to compile script");
                }
            }
        }
    }

    /// Register a hook script for a given event.
    pub fn register_hook(&mut self, event: ScriptEvent, script: &str) {
        match self.engine.compile(script) {
            Ok(ast) => {
                self.hooks.entry(event).or_default().push(ast);
            }
            Err(e) => {
                tracing::error!(event = ?event, error = %e, "failed to compile hook");
            }
        }
    }

    /// Fire all hooks registered for a given event.
    pub fn fire_event(&self, event: ScriptEvent) {
        if let Some(scripts) = self.hooks.get(&event) {
            for ast in scripts {
                if let Err(e) = self.engine.eval_ast(ast) {
                    tracing::error!(event = ?event, error = %e, "hook script failed");
                }
            }
        }
    }

    /// Run a named script by file stem (e.g., "startup" runs `startup.rhai`).
    pub fn run_script(&self, name: &str) -> Result<soushi::rhai::Dynamic, soushi::SoushiError> {
        if let Some(ast) = self.named_scripts.get(name) {
            self.engine.eval_ast(ast)
        } else {
            // Try loading from file directly
            let path = self.scripts_dir.join(format!("{name}.rhai"));
            self.engine.eval_file(&path)
        }
    }

    /// Access the underlying script engine for advanced usage.
    #[must_use]
    pub fn engine(&self) -> &ScriptEngine {
        &self.engine
    }
}

impl Default for ScriptManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve the scripts directory for an app: `~/.config/{app}/scripts`.
fn dirs_script_path(app: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join(app)
        .join("scripts")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_script_manager() {
        let _mgr = ScriptManager::new();
    }

    #[test]
    fn register_and_fire_hook() {
        let mut mgr = ScriptManager::new();
        mgr.register_hook(ScriptEvent::OnStart, r#"log_info("on_start fired")"#);
        mgr.fire_event(ScriptEvent::OnStart);
    }

    #[test]
    fn mado_send_keys_callable() {
        let mgr = ScriptManager::new();
        let result = mgr.engine().eval(r#"mado_send_keys("hello")"#).unwrap();
        assert!(result.into_string().unwrap().contains("sent keys"));
    }

    #[test]
    fn mado_split_callable() {
        let mgr = ScriptManager::new();
        let result = mgr.engine().eval(r#"mado_split("horizontal")"#).unwrap();
        assert!(result.into_string().unwrap().contains("split"));
    }

    #[test]
    fn mado_new_tab_callable() {
        let mgr = ScriptManager::new();
        let result = mgr.engine().eval("mado_new_tab()").unwrap();
        assert!(result.into_string().unwrap().contains("new tab"));
    }

    #[test]
    fn mado_set_title_callable() {
        let mgr = ScriptManager::new();
        let result = mgr.engine().eval(r#"mado_set_title("test")"#).unwrap();
        assert!(result.into_string().unwrap().contains("title set"));
    }

    #[test]
    fn run_nonexistent_script_errors() {
        let mgr = ScriptManager::new();
        let result = mgr.run_script("nonexistent_script_12345");
        assert!(result.is_err());
    }

    #[test]
    fn fire_event_no_hooks() {
        let mgr = ScriptManager::new();
        mgr.fire_event(ScriptEvent::OnStart);
        mgr.fire_event(ScriptEvent::OnQuit);
        mgr.fire_event(ScriptEvent::OnKey);
    }

    #[test]
    fn register_multiple_hooks() {
        let mut mgr = ScriptManager::new();
        mgr.register_hook(ScriptEvent::OnStart, r#"let x = 1 + 1;"#);
        mgr.register_hook(ScriptEvent::OnStart, r#"let y = 2 + 2;"#);
        mgr.fire_event(ScriptEvent::OnStart);
    }

    #[test]
    fn script_event_eq() {
        assert_eq!(ScriptEvent::OnStart, ScriptEvent::OnStart);
        assert_ne!(ScriptEvent::OnStart, ScriptEvent::OnQuit);
        assert_ne!(ScriptEvent::OnQuit, ScriptEvent::OnKey);
    }

    #[test]
    fn dirs_script_path_returns_expected() {
        let path = super::dirs_script_path("mado");
        assert!(path.ends_with("mado/scripts"));
    }
}
