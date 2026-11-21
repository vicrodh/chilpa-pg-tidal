//! Bounded concurrent JSON-RPC worker. Stdout belongs exclusively to the host.

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use tidal_plugin::service::Service;

const WORKERS: usize = 8;
const IN_FLIGHT: usize = 32;
const MAX_LINE: usize = 4 * 1024 * 1024;

struct Slots {
    used: Mutex<usize>,
    freed: Condvar,
}

impl Slots {
    fn acquire(&self) {
        let mut used = self.used.lock().unwrap();
        while *used >= IN_FLIGHT {
            used = self.freed.wait(used).unwrap();
        }
        *used += 1;
    }

    fn release(&self) {
        *self.used.lock().unwrap() -= 1;
        self.freed.notify_one();
    }
}

/// The data folder the host names: `data_dir` in `initialize`, else the
/// environment's `CHILPA_PLUGIN_DATA_DIR`. A missing or relative folder is an
/// error; no default is substituted. `cache_dir` is checked the same way.
fn folders(params: &Value) -> Result<PathBuf, String> {
    let folder = |key: &str, variable: &str| {
        let named = match params.get(key) {
            None | Some(Value::Null) => std::env::var_os(variable).map(PathBuf::from),
            Some(found) => found.as_str().map(PathBuf::from),
        };
        named.filter(|path| path.is_absolute())
            .ok_or_else(|| format!("initialize did not name an absolute {key}, and {variable} is not set to one"))
    };
    folder("cache_dir", "CHILPA_PLUGIN_CACHE_DIR")?;
    folder("data_dir", "CHILPA_PLUGIN_DATA_DIR")
}

fn reply_error(output: &Mutex<std::io::Stdout>, message: &Value, code: i64, text: &str) {
    if let Some(id) = message.get("id") {
        let line = json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}});
        let mut stdout = output.lock().unwrap();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

/// Opens storage in the folders `initialize` names, then answers it. It runs
/// on the reading thread, so no later request starts before storage exists.
fn initialize(started: &OnceLock<(PathBuf, Service)>, output: &Mutex<std::io::Stdout>, message: &Value) {
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let opened = folders(&params).and_then(|data| {
        if let Some((open, service)) = started.get() {
            return if *open == data { Ok(service) } else { Err(format!("its storage is already open in {}", open.display())) };
        }
        let service = Service::new(&data).map_err(|error| format!("{} could not be opened: {error}", data.display()))?;
        Ok(&started.get_or_init(|| (data, service)).1)
    });
    match opened {
        Ok(service) => call(service, output, message),
        Err(reason) => {
            eprintln!("The plugin could not start: {reason}");
            reply_error(output, message, -32602, &format!("The plugin could not start: {reason}"));
        }
    }
}

/// Every other request needs the storage `initialize` opened.
fn dispatch(started: &OnceLock<(PathBuf, Service)>, output: &Mutex<std::io::Stdout>, message: &Value) {
    match started.get() {
        Some((_, service)) => call(service, output, message),
        None => reply_error(output, message, -32002, "The plugin has not been initialized."),
    }
}

fn call(service: &Service, output: &Mutex<std::io::Stdout>, message: &Value) {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = match message.get("params") { Some(Value::Null) | None => json!({}), Some(params) => params.clone() };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| service.handle(method, &params)));
    let reply = match outcome {
        Ok(Ok(result)) => json!({"result": result}),
        Ok(Err(failure)) => {
            let mut error = json!({"code": failure.code, "message": failure.message});
            let mut detail = match failure.data { Some(Value::Object(fields)) => fields, _ => serde_json::Map::new() };
            if let Some(status) = failure.status {
                detail.insert("status".into(), json!(status));
            }
            if !detail.is_empty() {
                error["data"] = Value::Object(detail);
            }
            json!({"error": error})
        }
        Err(_) => json!({"error": {"code": -32000, "message": "The plugin could not complete this request. Refresh and try again."}}),
    };
    if let Some(id) = message.get("id") {
        let mut line = json!({"jsonrpc": "2.0", "id": id});
        if let (Value::Object(target), Value::Object(fields)) = (&mut line, reply) {
            target.extend(fields);
        }
        let mut stdout = output.lock().unwrap();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

fn main() {
    let started = Arc::new(OnceLock::<(PathBuf, Service)>::new());
    let output = Arc::new(Mutex::new(std::io::stdout()));
    let slots = Arc::new(Slots { used: Mutex::new(0), freed: Condvar::new() });
    let (sender, receiver) = mpsc::channel::<Value>();
    let receiver = Arc::new(Mutex::new(receiver));
    let workers: Vec<_> = (0..WORKERS).map(|index| {
        let (started, output, slots, receiver) = (started.clone(), output.clone(), slots.clone(), receiver.clone());
        std::thread::Builder::new().name(format!("rpc-{index}")).spawn(move || loop {
            let next = receiver.lock().unwrap().recv();
            let Ok(message) = next else { return };
            dispatch(&started, &output, &message);
            slots.release();
        }).expect("worker threads start")
    }).collect();
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > MAX_LINE {
            continue;
        }
        let Ok(message) = serde_json::from_slice::<Value>(&line) else { continue };
        if !message.is_object() {
            continue;
        }
        let method = message.get("method").and_then(Value::as_str);
        if method == Some("initialize") {
            initialize(&started, &output, &message);
            continue;
        }
        slots.acquire();
        // Playback events keep their order: they run as they arrive.
        if method == Some("playback.event") {
            dispatch(&started, &output, &message);
            slots.release();
        } else if sender.send(message).is_err() {
            slots.release();
            break;
        }
    }
    drop(sender);
    for worker in workers {
        let _ = worker.join();
    }
    if let Some((_, service)) = started.get() {
        service.close();
    }
}
