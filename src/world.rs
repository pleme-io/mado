use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use blue_lang_host::world::{self, Load, Refusal, Spec};
use blue_lang_host::{Canonical, Capability, Entry, Program, host};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{KeybindEntry, MadoConfig};
use mado::ibuki;

pub const WORLD_FUEL: usize = 20_000_000;
pub const COMMAND_FUEL: usize = 5_000_000;
pub const COMMAND_DEADLINE: Duration = Duration::from_secs(2);
pub const FIRST_WINDOW_PATIENCE: Duration = Duration::from_millis(400);

const ALLOW: &[Capability] = &[Capability::Environment, Capability::Clock];
const SPEC: Spec<'static> = Spec {
    call: "mado",
    kinds: "{**host_kinds, vigy: :open}",
    allow: ALLOW,
    fuel: WORLD_FUEL,
};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlay {
    pub fields: Vec<(String, Value)>,
    pub binds: Vec<(String, String)>,
    pub commands: HashMap<String, String>,
    pub vigies: HashMap<String, u64>,
    pub off_vigies: Vec<String>,
    pub refusals: Vec<Refusal>,
}

#[must_use]
pub fn world_file() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MADO_WORLD").map(PathBuf::from) {
        return Some(path);
    }
    config_home().map(|dir| dir.join("mado").join("mado.b"))
}

#[must_use]
pub fn vigies_dir() -> Option<PathBuf> {
    world_file().and_then(|f| f.parent().map(|p| p.join("vigies")))
}

fn config_home() -> Option<PathBuf> {
    let absolute = |var: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    absolute("XDG_CONFIG_HOME").or_else(|| absolute("HOME").map(|h| h.join(".config")))
}

/// Wall-clock milliseconds since the epoch — the worker's Flash clock.
fn wire_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

/// Frame one worker reply onto `out`: canonical JSON, Flash frames, the
/// host bucket pacing every spend. A frame the bucket still refuses after
/// its wait was waited out (a reply so large the 10 s parent deadline
/// cannot carry it) becomes a framed REFUSAL reply, so the channel stays
/// framed and the parent learns why.
fn wire_reply(
    out: &mut impl std::io::Write,
    result: &Result<Value, Refusal>,
) -> std::io::Result<()> {
    let reply = ibuki::reply_value(result);
    let waited = std::cell::Cell::new(0_u64);
    let mut send = |reply: &Value| -> std::io::Result<ibuki::Sent> {
        let mut sender = ibuki::ReplySender::new(out, ibuki::border_rate());
        sender.send(reply, wire_now_ms(), &mut |wait_ms| {
            waited.set(waited.get() + wait_ms);
            if waited.get() < 8_000 {
                std::thread::sleep(Duration::from_millis(wait_ms));
            }
        })
    };
    match send(&reply)? {
        ibuki::Sent::Written { .. } => Ok(()),
        ibuki::Sent::Refused { wait_ms, .. } => {
            let why = format!(
                "the reply is past the wire's Flash rate; it would still be sending after {} + {wait_ms} ms",
                waited.get()
            );
            send(&json!({ "refused": { "kind": "wire", "why": why } }))?;
            Ok(())
        }
    }
}

#[must_use]
pub fn serve_world(file: &Path) -> i32 {
    world::bound_memory(world::DEFAULT_MEMORY_MB);
    let result = world::judged::<CommandHost>(file, &SPEC);
    let stdout = std::io::stdout();
    i32::from(wire_reply(&mut stdout.lock(), &result).is_err())
}

#[must_use]
pub fn read_world(file: &Path) -> Load {
    if !file.exists() {
        return Load::Absent;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return Load::Refused(Refusal::new("spawn", e.to_string())),
    };
    let mut command = Command::new(exe);
    command
        .args(["blue-world", "--file"])
        .arg(file)
        .env("RUST_LOG", "off");
    ibuki::spawn_and_load(command, world::READ_LIMIT)
}

fn set_fields(overlay: &mut Overlay, group: &str, fields: &Value) {
    if let Some(map) = fields.as_object() {
        for (key, value) in map {
            overlay.fields.push((
                format!("{group}.{key}"),
                json!({ group: { key.as_str(): value } }),
            ));
        }
    }
}

fn read_entry(entry: &Value, overlay: &mut Overlay) {
    let kind = entry.get("entry").and_then(world::word).unwrap_or_default();
    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let value = entry.get("value").cloned().unwrap_or_else(|| json!({}));
    match kind {
        "config" => set_fields(overlay, name, &value),
        "font" => {
            if let Some(map) = value.as_object() {
                for (key, field) in map {
                    overlay
                        .fields
                        .push((key.clone(), json!({ key.as_str(): field })));
                }
            }
        }
        "bind" => {
            let action = value
                .get("action")
                .and_then(world::word)
                .unwrap_or_default();
            overlay.binds.push((name.to_owned(), action.to_owned()));
        }
        "command" => {
            let call = value
                .get("call")
                .and_then(world::word)
                .unwrap_or(name)
                .to_owned();
            overlay.commands.insert(name.to_owned(), call);
        }
        "vigy" => {
            let every = value.get("every_ms").and_then(Value::as_u64).unwrap_or(0);
            overlay.vigies.insert(name.to_owned(), every);
        }
        other => overlay.refusals.push(Refusal::new(
            "invalid",
            format!("{other} {name}: no such kind"),
        )),
    }
}

#[must_use]
pub fn overlay_of(load: &Load) -> Overlay {
    let mut overlay = Overlay::default();
    match load {
        Load::Absent => {}
        Load::Refused(refusal) => overlay.refusals.push(refusal.clone()),
        Load::Judged(judged) => {
            overlay.refusals = world::refusals_of(judged);
            for entry in world::listed(judged, "off") {
                if entry.get("entry").and_then(world::word) == Some("vigy") {
                    if let Some(name) = entry.get("name").and_then(Value::as_str) {
                        overlay.off_vigies.push(name.to_owned());
                    }
                }
            }
            for entry in world::listed(judged, "accepted") {
                read_entry(&entry, &mut overlay);
            }
        }
    }
    overlay
}

impl Overlay {
    #[must_use]
    pub fn apply(&self, mut config: MadoConfig) -> (MadoConfig, Vec<Refusal>) {
        let mut refusals = Vec::new();
        for (path, over) in &self.fields {
            match world::merged(&config, over) {
                Ok(next) => config = next,
                Err(why) => refusals.push(Refusal::new("invalid", format!("{path}: {why}"))),
            }
        }
        for (trigger, action) in &self.binds {
            let known = crate::keybind::parse_action(action).is_some();
            if let Err(e) = awase::Hotkey::parse(trigger) {
                refusals.push(Refusal::new("invalid", format!("bind {trigger}: {e}")));
            } else if known {
                config.keybinds.custom.push(KeybindEntry {
                    trigger: trigger.clone(),
                    action: action.clone(),
                });
            } else if !self.commands.contains_key(action) {
                refusals.push(Refusal::new(
                    "invalid",
                    format!("bind {trigger}: {action} is no action and no command"),
                ));
            }
        }
        (config, refusals)
    }

    #[must_use]
    pub fn command_on(&self, hotkey: &awase::Hotkey) -> Option<&str> {
        self.binds.iter().rev().find_map(|(trigger, action)| {
            let call = self.commands.get(action)?;
            (awase::Hotkey::parse(trigger).ok()? == *hotkey).then_some(call.as_str())
        })
    }
}

fn current() -> &'static RwLock<Arc<Overlay>> {
    static CURRENT: OnceLock<RwLock<Arc<Overlay>>> = OnceLock::new();
    CURRENT.get_or_init(|| RwLock::new(Arc::new(Overlay::default())))
}

pub fn install(overlay: Overlay) {
    publish_refusals(&overlay.refusals);
    *current()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(overlay);
}

#[must_use]
pub fn installed() -> Arc<Overlay> {
    Arc::clone(
        &current()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

#[must_use]
pub fn over(config: MadoConfig) -> MadoConfig {
    let overlay = installed();
    let (config, refusals) = overlay.apply(config);
    publish_refusals(&refusals);
    config
}

fn publish_refusals(refusals: &[Refusal]) {
    for refusal in refusals {
        tracing::warn!(world = %refusal, "mado.b");
        let mut title = String::from("mado.b: ");
        title.push_str(&refusal.why);
        let params = crate::suggest::InjectParams {
            title,
            key: Some(format!("mado.b {refusal}")),
            detail: Some(refusal.kind.clone()),
            urgency: Some(String::from("high")),
            cwd: None,
            session_name: None,
            command: None,
        };
        if let Err(e) = crate::suggest::inject(params) {
            tracing::warn!(error = %e, "could not show a mado.b refusal on the board");
        }
    }
}

pub fn first_load(file: Option<PathBuf>, patience: Duration, late: impl FnOnce() + Send + 'static) {
    let Some(file) = file.filter(|f| f.exists()) else {
        return;
    };
    let first = world::within(
        patience,
        move || overlay_of(&read_world(&file)),
        move |overlay| {
            install(overlay);
            late();
        },
    );
    if let Some(overlay) = first {
        install(overlay);
    }
}

pub fn watch(file: PathBuf, dirty: Arc<AtomicBool>) {
    use notify::{RecursiveMode, Watcher};
    let Some(dir) = file.parent().map(Path::to_path_buf) else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("mado-world".to_owned())
        .spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            let Ok(mut watcher) = notify::recommended_watcher(tx) else {
                return;
            };
            if std::fs::create_dir_all(&dir).is_err()
                || watcher.watch(&dir, RecursiveMode::Recursive).is_err()
            {
                return;
            }
            while let Ok(event) = rx.recv() {
                let Ok(event) = event else { continue };
                if event.kind.is_access() {
                    continue;
                }
                while rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
                install(overlay_of(&read_world(&file)));
                crate::vigy_host::sync_soon();
                dirty.store(true, Ordering::Release);
                crate::ring::WINDOW.ring();
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "no thread to watch mado.b");
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    SendText(String),
    RunAction(String),
    Board { key: String, title: String },
}

#[derive(Default)]
struct CommandHost {
    effects: Vec<Effect>,
}

#[host]
impl CommandHost {
    /// Type text into the focused pane.
    fn send_text(&mut self, text: String) {
        self.effects.push(Effect::SendText(text));
    }

    /// Run a keybinding action by its name.
    fn run_action(&mut self, name: String) {
        self.effects.push(Effect::RunAction(name));
    }

    /// Put a row on the Ctrl-S board; the same key updates the row.
    fn board(&mut self, key: String, title: String) {
        self.effects.push(Effect::Board { key, title });
    }
}

#[must_use]
pub fn serve_command(file: &Path, call: &str) -> i32 {
    world::bound_memory(world::DEFAULT_MEMORY_MB);
    let result = command_effects(file, call)
        .map(|effects| json!({ "effects": effects }))
        .map_err(|why| Refusal::new("command", why));
    let stdout = std::io::stdout();
    i32::from(wire_reply(&mut stdout.lock(), &result).is_err())
}

fn command_effects(file: &Path, call: &str) -> Result<Vec<Effect>, String> {
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let loader = world::loader_for(file);
    let entry = Entry {
        path: Some(file),
        text: &text,
    };
    let mut host = CommandHost::default();
    let permits = |_: &str| true;
    let mut program = Program::<CommandHost>::builder(entry)
        .loader(&loader)
        .canonical(Canonical::AsWritten)
        .posture(ALLOW)
        .frame(&permits, world::budget(COMMAND_FUEL))
        .load(&mut host)
        .map_err(|e| e.to_string())?;
    host.effects.clear();
    program
        .call_budgeted::<blue_lang_host::Value>(call, (), &mut host, world::budget(COMMAND_FUEL))
        .map_err(|e| e.to_string())?;
    Ok(host.effects)
}

fn effects_from(stdout: &[u8]) -> Result<Vec<Effect>, String> {
    let reply = framed_reply(stdout).or_else(|| {
        let text = String::from_utf8_lossy(stdout);
        text.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .and_then(|l| serde_json::from_str::<Value>(l).ok())
    });
    let reply: Value = reply.ok_or_else(|| String::from("the command worker gave no answer"))?;
    if let Some(why) = reply.get("refused").and_then(Value::as_str) {
        return Err(why.to_owned());
    }
    if let Some(refused) = reply.get("refused").and_then(Value::as_object) {
        let kind = refused
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("command");
        let why = refused.get("why").and_then(Value::as_str).unwrap_or("");
        return Err(format!("{kind}: {why}"));
    }
    serde_json::from_value(reply.get("effects").cloned().unwrap_or(Value::Null))
        .map_err(|e| e.to_string())
}

/// The reply `Value` of a framed worker answer, or `None` when the stream
/// is not framed (the former plain one-line answer).
fn framed_reply(stdout: &[u8]) -> Option<Value> {
    let payload = ibuki::framed_payload(stdout).ok()?;
    serde_json::from_str(String::from_utf8_lossy(&payload).trim()).ok()
}

fn run_command_child(file: &Path, call: &str) -> Result<Vec<Effect>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(exe);
    command
        .args(["blue-command", "--file"])
        .arg(file)
        .args(["--call", call])
        .env("RUST_LOG", "off");
    match world::run_bounded(command, COMMAND_DEADLINE) {
        world::Bounded::Finished { stdout, status } => {
            effects_from(&stdout).map_err(|why| format!("{why} ({status})"))
        }
        world::Bounded::TimedOut => {
            Err(format!("stopped after {} ms", COMMAND_DEADLINE.as_millis()))
        }
        world::Bounded::Failed(e) => Err(e.to_string()),
    }
}

fn pending() -> &'static Mutex<VecDeque<Effect>> {
    static PENDING: OnceLock<Mutex<VecDeque<Effect>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(VecDeque::new()))
}

pub fn spawn_command(call: String) {
    let Some(file) = world_file() else { return };
    let spawned = std::thread::Builder::new()
        .name("mado-blue-command".to_owned())
        .spawn(move || match run_command_child(&file, &call) {
            Ok(effects) => {
                pending()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(effects);
                crate::ring::WINDOW.ring();
            }
            Err(why) => publish_refusals(&[Refusal::new("command", format!("{call}: {why}"))]),
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "no thread for a blue command");
    }
}

#[must_use]
pub fn take_effects() -> Vec<Effect> {
    pending()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..)
        .collect()
}

pub fn board_row(key: String, title: String) {
    let params = crate::suggest::InjectParams {
        title,
        key: Some(key),
        detail: None,
        urgency: None,
        cwd: None,
        session_name: None,
        command: None,
    };
    if let Err(e) = crate::suggest::inject(params) {
        tracing::warn!(error = %e, "a blue board row was refused");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_reply_of(reply: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        let mut no_wait = |_ms: u64| {};
        ibuki::ReplySender::new(&mut out, ibuki::border_rate())
            .send(reply, 0, &mut no_wait)
            .expect("a reply fits the border");
        out
    }

    #[test]
    fn effects_read_back_from_flash_frames_and_from_the_plain_line() {
        let effects = vec![
            Effect::SendText("echo hi\n".into()),
            Effect::RunAction("scroll_to_top".into()),
            Effect::Board {
                key: "clock".into(),
                title: "tick".into(),
            },
        ];
        let reply = json!({ "effects": effects });
        let framed = frame_reply_of(&reply);
        assert_eq!(effects_from(&framed).unwrap(), effects);
        let plain = format!("{}\n", serde_json::to_string(&reply).unwrap());
        assert_eq!(effects_from(plain.as_bytes()).unwrap(), effects);
    }

    #[test]
    fn a_framed_refusal_keeps_its_kind_and_why_and_a_plain_line_still_reads() {
        let refused = frame_reply_of(&ibuki::reply_value(&Err(Refusal::new(
            "load",
            "no such word",
        ))));
        assert_eq!(
            ibuki::framed_load(&refused),
            Some(Load::Refused(Refusal::new("load", "no such word")))
        );
        let judged = frame_reply_of(&ibuki::reply_value(&Ok(json!({"font": {"size": 14}}))));
        assert_eq!(
            ibuki::framed_load(&judged),
            Some(Load::Judged(json!({"font": {"size": 14}})))
        );
        assert_eq!(ibuki::framed_load(b"{\"judged\": {\"a\": 1}}\n"), None);
    }

    fn judged(accepted: Value) -> Load {
        Load::Judged(json!({"accepted": accepted, "off": [], "refusals": []}))
    }

    fn entry(kind: &str, name: &str, value: Value) -> Value {
        json!({"entry": format!(":{kind}"), "name": name, "value": value})
    }

    #[test]
    fn no_world_leaves_the_yaml_config_untouched() {
        let yaml = MadoConfig::default();
        let (config, refused) = overlay_of(&Load::Absent).apply(yaml.clone());
        assert!(refused.is_empty());
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::to_value(&yaml).unwrap()
        );
    }

    #[test]
    fn a_bad_field_is_refused_alone_and_its_siblings_apply() {
        let (config, refused) = overlay_of(&judged(json!([entry(
            "config",
            "appearance",
            json!({"opacity": 0.5, "brand_new": 1, "background": 7})
        )])))
        .apply(MadoConfig::default());
        assert!((config.appearance.opacity - 0.5).abs() < f32::EPSILON);
        assert_eq!(refused.len(), 2, "{refused:?}");
        assert!(
            refused
                .iter()
                .any(|r| r.why.starts_with("appearance.brand_new:"))
        );
    }

    #[test]
    fn a_world_value_wins_over_yaml_and_a_nested_group_keeps_its_siblings() {
        let mut yaml = MadoConfig::default();
        yaml.font_size = 11.0;
        yaml.window.chrome.height = 33.0;
        yaml.window.chrome.tint = 0.25;
        let (config, refused) = overlay_of(&judged(json!([
            entry("font", "font", json!({"font_size": 19})),
            entry("config", "window", json!({"chrome": {"height": 44}})),
        ])))
        .apply(yaml);
        assert!(refused.is_empty(), "{refused:?}");
        assert!((config.font_size - 19.0).abs() < f32::EPSILON);
        assert!((config.window.chrome.height - 44.0).abs() < f32::EPSILON);
        assert!((config.window.chrome.tint - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn motion_values_reach_their_group() {
        let (config, refused) = overlay_of(&judged(json!([entry(
            "config",
            "motion",
            json!({"bell_flash": {"duration_ms": 77}})
        )])))
        .apply(MadoConfig::default());
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(config.motion.bell_flash.duration_ms, 77);
    }

    #[test]
    fn a_binding_reaches_the_manager_and_wins_over_a_default() {
        let overlay = overlay_of(&judged(json!([
            entry("bind", "cmd+shift+y", json!({"action": ":scroll_to_top"})),
            entry("bind", "cmd+c", json!({"action": "paste"})),
        ])));
        let (config, refused) = overlay.apply(MadoConfig::default());
        assert!(refused.is_empty(), "{refused:?}");
        let manager = crate::keybind::manager_from_config(&config);
        let on = |chord: &str| manager.lookup(&awase::Hotkey::parse(chord).unwrap());
        assert_eq!(on("cmd+shift+y"), Some(crate::keybind::Action::ScrollToTop));
        assert_eq!(on("cmd+c"), Some(crate::keybind::Action::Paste));
    }

    #[test]
    fn a_bad_binding_is_refused_alone() {
        let overlay = overlay_of(&judged(json!([
            entry("bind", "nonsense+key+x", json!({"action": "copy"})),
            entry("bind", "cmd+j", json!({"action": "no_such"})),
            entry("bind", "cmd+k", json!({"action": "copy"})),
        ])));
        let (config, refused) = overlay.apply(MadoConfig::default());
        assert_eq!(refused.len(), 2, "{refused:?}");
        assert_eq!(config.keybinds.custom.len(), 1);
    }

    #[test]
    fn a_binding_to_a_command_runs_its_function_and_never_reaches_the_manager() {
        let overlay = overlay_of(&judged(json!([
            entry("command", "hello", json!({"call": "greet"})),
            entry("command", "plain", json!({})),
            entry("bind", "cmd+shift+y", json!({"action": "hello"})),
            entry("bind", "cmd+shift+u", json!({"action": "plain"})),
        ])));
        let (config, refused) = overlay.apply(MadoConfig::default());
        assert!(refused.is_empty(), "{refused:?}");
        assert!(config.keybinds.custom.is_empty());
        let on = |chord: &str| {
            overlay
                .command_on(&awase::Hotkey::parse(chord).unwrap())
                .map(str::to_owned)
        };
        assert_eq!(on("cmd+shift+y").as_deref(), Some("greet"));
        assert_eq!(on("cmd+shift+u").as_deref(), Some("plain"));
        assert_eq!(on("cmd+shift+i"), None);
    }

    #[test]
    fn a_refused_world_keeps_the_yaml_and_says_why() {
        let overlay = overlay_of(&Load::Refused(Refusal::new("load", "parse error")));
        let (config, _) = overlay.apply(MadoConfig::default());
        assert_eq!(overlay.refusals[0].why, "parse error");
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::to_value(MadoConfig::default()).unwrap()
        );
    }

    #[test]
    fn a_real_world_file_is_judged_with_every_word() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mado.b");
        std::fs::write(
            &path,
            "use(\"motenashi\", %i[bind command config font look motion off])\n\ndef mado()\n  [look({opacity: 0.5, nope: 1}), font(\"Inter\", 17), motion({bell_flash: {duration_ms: 9}}), config(:cursor, {blink: false}), bind(\"cmd+j\", :copy), bind(\"cmd+k\", \"hi\"), command(\"hi\", {}), bind(\"x\", 7), off(:config, \"cursor\")]\nend\n",
        )
        .unwrap();
        let judged = world::judged::<CommandHost>(&path, &SPEC).unwrap();
        let overlay = overlay_of(&Load::Judged(judged));
        assert_eq!(overlay.refusals.len(), 1, "{:?}", overlay.refusals);
        let (config, refused) = overlay.apply(MadoConfig::default());
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!((config.font_size - 17.0).abs() < f32::EPSILON);
        assert_eq!(config.motion.bell_flash.duration_ms, 9);
        assert_eq!(config.font_family, "Inter");
    }

    #[test]
    fn a_command_returns_its_effects_and_a_loop_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mado.b");
        std::fs::write(
            &path,
            "send_text(\"top level\")\n\ndef hi()\n  send_text(\"ls\\n\")\n  run_action(\"copy\")\n  board(\"k\", \"row\")\nend\n\ndef spin()\n  while(true, 1)\nend\n",
        )
        .unwrap();
        assert_eq!(
            command_effects(&path, "hi").unwrap(),
            vec![
                Effect::SendText("ls\n".into()),
                Effect::RunAction("copy".into()),
                Effect::Board {
                    key: "k".into(),
                    title: "row".into()
                },
            ]
        );
        assert!(command_effects(&path, "spin").is_err());
        assert!(command_effects(&path, "absent").is_err());
    }

    #[test]
    fn a_refusal_reaches_the_board() {
        publish_refusals(&[Refusal::new("invalid", "unique-refusal-row")]);
        let found = crate::suggest::store()
            .ranked_stored(1_000, crate::suggest::now_unix_ms())
            .iter()
            .any(|s| s.item.title.contains("unique-refusal-row"));
        assert!(found);
    }
}
