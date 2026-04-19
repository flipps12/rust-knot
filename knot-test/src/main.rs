// src/main.rs  — mirrors the TypeScript test_main.ts exactly
//
// Usage:
//   cargo run --bin test_main
//
// Flow:
//   1. Connect KnotClient
//   2. Register as "jstext" on port 8124
//   3. Print incoming JSON messages and byte payloads
//   4. Ask for a peer ID, then loop reading lines → send_bytes

use knot_sdk::{ KnotClient, KnotCommand };
use std::io::{ self, BufRead, Write };

#[tokio::main]
async fn main() {
    println!("{}", "=======".to_owned() + " Main test " + "=======");

    // ── Connect ──────────────────────────────────────────────────────────
    let knot = match KnotClient::new().await {
        Ok(k) => k,
        Err(e) => {
            eprintln!("Failed to connect KnotClient: {e}");
            std::process::exit(1);
        }
    };

    // ── Register app ─────────────────────────────────────────────────────
    knot.send_json(KnotCommand::NewAppName {
        name: "rstext".into(),
        port: 8125,
    }).await.expect("send_json failed");

    knot.send_json(KnotCommand::Protocol).await.expect("send_json failed");
    knot.send_json(KnotCommand::GetCommands).await.expect("send_json failed");
    knot.send_json(KnotCommand::GetPeerId).await.expect("send_json failed");

    // ── Listeners (background tasks) ─────────────────────────────────────

    // JSON messages  →  knot.on('message', ...)
    let mut msg_rx = knot.subscribe_messages();
    tokio::spawn(async move {
        loop {
            match msg_rx.recv().await {
                Ok(msg) => {
                    println!("Mensaje entrante: {:?}", msg);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("[Knot] message channel lagged, skipped {n} messages");
                }
                Err(_) => {
                    break;
                }
            }
        }
    });

    // Byte payloads  →  knot.on('byte', ...)
    let mut byte_rx = knot.subscribe_bytes();
    tokio::spawn(async move {
        loop {
            match byte_rx.recv().await {
                Ok(msg) => {
                    println!("Peer: {msg}");
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("[Knot] byte channel lagged, skipped {n} messages");
                }
                Err(_) => {
                    break;
                }
            }
        }
    });

    // ── Interactive loop (mirrors mainLoop() in TS) ───────────────────────
    // We use blocking stdin on a dedicated thread to avoid blocking the async runtime.
    let knot_clone = knot.clone();
    tokio::task
        ::spawn_blocking(move || {
            let stdin = io::stdin();
            let stdout = io::stdout();

            print!("Connect to Peer ID: ");
            stdout.lock().flush().unwrap();

            let mut lines = stdin.lock().lines();

            let peer = match lines.next() {
                Some(Ok(p)) => p,
                _ => {
                    eprintln!("No peer ID provided, exiting.");
                    return;
                }
            };

            println!("Conectado a {peer}. Escribí tus mensajes:");

            let rt = tokio::runtime::Handle::current();

            loop {
                print!("> ");
                stdout.lock().flush().unwrap();

                match lines.next() {
                    Some(Ok(message)) => {
                        if message.trim().to_lowercase() == "quit" {
                            break;
                        }

                        let payload = message.as_bytes().to_vec();
                        let peer_clone = peer.clone();
                        let knot_inner = knot_clone.clone();

                        if
                            let Err(e) = rt.block_on(async move {
                                knot_inner.send_bytes(&peer_clone, &payload).await
                            })
                        {
                            eprintln!("Fallo en el SDK: {e}");
                        }
                    }
                    _ => {
                        break;
                    }
                }
            }

            println!("Cerrando.");
        }).await
        .expect("spawn_blocking panicked");
}
