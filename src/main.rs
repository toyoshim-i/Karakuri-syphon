//! A Syphon output sink plugin that speaks Karakuri's output-plugin protocol.
//!
//! One line of ndjson per message on stdout; messages read from stdin; diagnostics
//! and human-readable logging on stderr. The protocol is Karakuri's and is specified
//! in its `output_plugin` module — this program implements that specification
//! independently rather than importing types from it.

use std::io::{BufRead, BufReader, Write};
use serde::{Deserialize, Serialize};

const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum HostMessage {
    Open {
        surface: String,
        width: u32,
        height: u32,
        format: String,
    },
    Frame {
        index: u64,
        surface_id: u32,
        width: u32,
        height: u32,
    },
    Resize {
        width: u32,
        height: u32,
    },
    Close,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum PluginMessage {
    Hello {
        v: u32,
        kind: String,
        name: String,
        surfaces: Vec<String>,
    },
    Ready {
        server_name: String,
    },
    Status {
        clients: u32,
        dropped: u64,
    },
}

fn emit(msg: &PluginMessage) {
    if let Ok(line) = serde_json::to_string(msg) {
        println!("{line}");
        let _ = std::io::stdout().flush();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut server_name = "Karakuri".to_string();
    while let Some(arg) = args.next() {
        if arg == "--name" {
            if let Some(name) = args.next() {
                server_name = name;
            }
        }
    }

    // 1. Initial greeting
    emit(&PluginMessage::Hello {
        v: PROTOCOL_VERSION,
        kind: "output".to_string(),
        name: "syphon".to_string(),
        surfaces: vec!["iosurface".to_string()],
    });

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();

    // 2. Await Open configuration
    let (mut current_width, mut current_height);
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            eprintln!("karakuri-syphon: stdin closed before Open configuration");
            return;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<HostMessage>(trimmed) {
            Ok(HostMessage::Open {
                surface,
                width,
                height,
                format,
            }) => {
                if surface != "iosurface" {
                    eprintln!("karakuri-syphon: unsupported surface `{surface}` requested");
                    return;
                }
                current_width = width;
                current_height = height;
                eprintln!(
                    "karakuri-syphon: configured for {width}x{height} {format} (server: {server_name})"
                );
                emit(&PluginMessage::Ready {
                    server_name: server_name.clone(),
                });
                break;
            }
            Ok(HostMessage::Close) => return,
            Ok(other) => {
                eprintln!("karakuri-syphon: ignoring unexpected message before Open: {other:?}");
            }
            Err(e) => {
                eprintln!("karakuri-syphon: failed to parse initial message: {e}");
            }
        }
    }

    // 3. Main frame loop
    let mut frame_count = 0u64;
    let mut last_status = std::time::Instant::now();

    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<HostMessage>(trimmed) {
            Ok(HostMessage::Frame {
                index,
                surface_id,
                width,
                height,
            }) => {
                frame_count += 1;
                // Here the Syphon framework integration publishes the IOSurfaceID.
                // For now in the protocol skeleton, we track frame arrival.
                if current_width != width || current_height != height {
                    current_width = width;
                    current_height = height;
                }
                if last_status.elapsed() >= std::time::Duration::from_secs(1) {
                    emit(&PluginMessage::Status {
                        clients: 0,
                        dropped: 0,
                    });
                    last_status = std::time::Instant::now();
                    eprintln!(
                        "karakuri-syphon: streaming active (frame {index}, surface_id {surface_id}, total {frame_count})"
                    );
                }
            }
            Ok(HostMessage::Resize { width, height }) => {
                current_width = width;
                current_height = height;
                eprintln!("karakuri-syphon: resized to {width}x{height}");
            }
            Ok(HostMessage::Close) => {
                eprintln!("karakuri-syphon: received Close command, shutting down");
                break;
            }
            Ok(HostMessage::Unknown) => {}
            Err(e) => {
                eprintln!("karakuri-syphon: unparseable line: {e}");
            }
            _ => {}
        }
    }
}
