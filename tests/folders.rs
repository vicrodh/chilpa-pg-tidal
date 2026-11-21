//! The process keeps its files in the folders the host names, and refuses to
//! start without them instead of guessing one.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

struct Plugin {
    child: Child,
    output: BufReader<ChildStdout>,
    serial: u64,
}

impl Plugin {
    /// Starts the executable with an empty environment apart from `variables`,
    /// a working directory and a home that are not the data folder.
    fn start(root: &Path, variables: &[(&str, &Path)]) -> Plugin {
        let (work, home) = (root.join("work"), root.join("home"));
        for folder in [&work, &home] {
            std::fs::create_dir_all(folder).unwrap();
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_plugin"));
        command.env_clear().env("HOME", &home).env("XDG_CACHE_HOME", &home).current_dir(&work)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        for (name, value) in variables {
            command.env(name, value);
        }
        let mut child = command.spawn().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Plugin { child, output, serial: 0 }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.serial += 1;
        let line = json!({"jsonrpc": "2.0", "id": self.serial, "method": method, "params": params});
        let input = self.child.stdin.as_mut().unwrap();
        writeln!(input, "{line}").unwrap();
        input.flush().unwrap();
        let mut answer = String::new();
        self.output.read_line(&mut answer).unwrap();
        let answer: Value = serde_json::from_str(&answer).unwrap_or_else(|_| panic!("{method} got no JSON answer: {answer:?}"));
        assert_eq!(answer["id"], json!(self.serial));
        answer
    }

    /// Closes the input and waits for the process to end on its own.
    fn finish(mut self) {
        drop(self.child.stdin.take());
        assert!(self.child.wait().unwrap().success(), "the process ends cleanly when the host closes its input");
    }
}

/// Every file under `root`, relative to it.
fn files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(folder) = pending.pop() {
        for entry in std::fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }
    found.sort();
    found
}

/// Opens the settings page and saves a preference, which writes a file.
fn save_a_preference(plugin: &mut Plugin) {
    let page = plugin.call("settings.open", json!({}));
    let revision = page["result"]["revision"].clone();
    assert!(!revision.is_null(), "settings.open answered a page: {page}");
    let saved = plugin.call("settings.submit", json!({"revision": revision, "action": "quality_LOSSLESS"}));
    assert!(saved.get("result").is_some(), "the preference was saved: {saved}");
}

#[test]
fn it_writes_only_in_the_data_folder_initialize_names() {
    let root = tempfile::tempdir().unwrap();
    let (data, cache) = (root.path().join("profile/plugin-data"), root.path().join("profile/plugin-cache"));
    let mut plugin = Plugin::start(root.path(), &[]);
    let greeting = plugin.call("initialize", json!({"protocol": 1, "data_dir": data, "cache_dir": cache, "locale": "en"}));
    assert_eq!(greeting["result"]["protocol"], json!(1), "{greeting}");
    save_a_preference(&mut plugin);
    plugin.finish();
    assert!(data.join("preferences.json").is_file(), "the preference is in the data folder");
    let outside: Vec<_> = files(root.path()).into_iter().filter(|path| !path.starts_with("profile/plugin-data")).collect();
    assert!(outside.is_empty(), "nothing is written outside the data folder: {outside:?}");
}

#[test]
fn without_a_folder_from_initialize_it_reads_the_environment() {
    let root = tempfile::tempdir().unwrap();
    let (data, cache) = (root.path().join("variables/data"), root.path().join("variables/cache"));
    let mut plugin = Plugin::start(root.path(), &[("CHILPA_PLUGIN_DATA_DIR", &data), ("CHILPA_PLUGIN_CACHE_DIR", &cache)]);
    let greeting = plugin.call("initialize", json!({"protocol": 1, "locale": "en"}));
    assert_eq!(greeting["result"]["protocol"], json!(1), "{greeting}");
    save_a_preference(&mut plugin);
    plugin.finish();
    assert!(data.join("preferences.json").is_file(), "the preference is in the folder the environment names");
}

#[test]
fn without_any_folder_it_refuses_to_start_and_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    for params in [json!({"protocol": 1, "locale": "en"}),
                   json!({"protocol": 1, "data_dir": "plugin-data", "cache_dir": "plugin-cache"})] {
        let mut plugin = Plugin::start(root.path(), &[]);
        let greeting = plugin.call("initialize", params.clone());
        let message = greeting["error"]["message"].as_str().unwrap_or_else(|| panic!("initialize {params} failed: {greeting}"));
        assert!(message.contains("cache_dir") || message.contains("data_dir"), "the error names the missing folder: {message}");
        let refused = plugin.call("auth.status", json!({}));
        assert_eq!(refused["error"]["code"], json!(-32002), "nothing runs before a successful initialize: {refused}");
        plugin.finish();
    }
    assert!(files(root.path()).is_empty(), "no default folder was created: {:?}", files(root.path()));
}
