use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use blue_lang_host::world::{self, Load};

fn scratch(text: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mado.b");
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

fn worker(path: &PathBuf) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mado"));
    command.args(["blue-world", "--file"]).arg(path);
    command.env("RUST_LOG", "off");
    command
}

#[test]
fn the_binary_judges_a_world_and_refuses_a_bad_entry_alone() {
    let (_d, path) = scratch(
        "use(\"motenashi\", %i[bind look])\n\ndef mado()\n  [look({opacity: 0.5}), bind(\"x\", 7)]\nend\n",
    );
    let Load::Judged(judged) = world::read(worker(&path), &path, world::READ_LIMIT) else {
        panic!("a world must be judged");
    };
    assert_eq!(world::listed(&judged, "accepted").len(), 1);
    assert_eq!(world::refusals_of(&judged).len(), 1);
}

#[test]
fn a_world_whose_command_names_a_host_word_is_judged() {
    let (_d, path) = scratch(
        "use(\"motenashi\", [:command])\n\ndef mado()\n  [command(\"hello\", {})]\nend\n\ndef hello()\n  send_text(\"echo hello\\n\")\nend\n",
    );
    let load = world::read(worker(&path), &path, world::READ_LIMIT);
    let Load::Judged(judged) = load else {
        panic!("{load:?}");
    };
    assert_eq!(world::listed(&judged, "accepted").len(), 1);
}

#[test]
fn a_loop_and_a_native_runaway_cannot_hold_the_window() {
    let (_d, spin) = scratch("def mado()\n  while(true, 1)\nend\n");
    let Load::Refused(r) = world::read(worker(&spin), &spin, world::READ_LIMIT) else {
        panic!("a loop must be refused");
    };
    assert_eq!(r.kind, "exhausted");

    let (_d, native) =
        scratch("def mado()\n  xs = range(0, 100000000000000)\n  [length(xs)]\nend\n");
    let started = Instant::now();
    let limit = Duration::from_secs(2);
    let Load::Refused(r) = world::read(worker(&native), &native, limit) else {
        panic!("a native runaway must be refused");
    };
    assert_eq!(r.kind, "killed");
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn a_command_prints_its_effects_and_the_file_needs_no_formatting() {
    let (_d, path) = scratch("def hi(  )\n  send_text(\"ls\\n\")\nend\n");
    let out = Command::new(env!("CARGO_BIN_EXE_mado"))
        .args(["blue-command", "--file"])
        .arg(&path)
        .args(["--call", "hi"])
        .env("RUST_LOG", "off")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("SendText"), "{text}");
}
