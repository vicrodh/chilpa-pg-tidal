//! Bounded concurrent JSON-RPC worker. Stdout belongs exclusively to the host.

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Condvar, Mutex};
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

fn data_dir() -> PathBuf {
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--data-dir" {
            if let Some(path) = arguments.next() {
                return PathBuf::from(path);
            }
        } else if let Some(path) = argument.strip_prefix("--data-dir=") {
            return PathBuf::from(path);
        }
    }
    std::env::var_os("CHILPA_PLUGIN_DATA_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/data"))
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
    let service = match Service::new(&data_dir()) {
        Ok(service) => Arc::new(service),
        Err(error) => {
            eprintln!("The plugin could not start: {error}");
            std::process::exit(1);
        }
    };
    let output = Arc::new(Mutex::new(std::io::stdout()));
    let slots = Arc::new(Slots { used: Mutex::new(0), freed: Condvar::new() });
    let (sender, receiver) = mpsc::channel::<Value>();
    let receiver = Arc::new(Mutex::new(receiver));
    let workers: Vec<_> = (0..WORKERS).map(|index| {
        let (service, output, slots, receiver) = (service.clone(), output.clone(), slots.clone(), receiver.clone());
        std::thread::Builder::new().name(format!("rpc-{index}")).spawn(move || loop {
            let next = receiver.lock().unwrap().recv();
            let Ok(message) = next else { return };
            call(&service, &output, &message);
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
        slots.acquire();
        // Playback events keep their order: they run as they arrive.
        if message.get("method").and_then(Value::as_str) == Some("playback.event") {
            call(&service, &output, &message);
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
    service.close();
}
