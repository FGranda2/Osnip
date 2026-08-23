//! Human-readable rendering of `IpcResponse` to stdout.
//!
//! Output is plain text (no colors, no JSON) so it composes cleanly with
//! shell pipelines. Errors go to stderr; non-error responses go to
//! stdout. A future `--json` flag can layer onto this without changing
//! the wire protocol.

use osnip_core::{IpcRequest, IpcResponse};

pub fn print_response(request: &IpcRequest, response: &IpcResponse) {
    match response {
        IpcResponse::Ok => {
            // For `close` / `close_all` a silent success is the most
            // shell-friendly outcome — the exit code carries the signal.
            if !matches!(request, IpcRequest::Close { .. } | IpcRequest::CloseAll) {
                println!("ok");
            }
        }
        IpcResponse::Pinned { id } => {
            println!("{id}");
        }
        IpcResponse::Pins { pins } => {
            if pins.is_empty() {
                println!("no pins");
                return;
            }
            println!("{:>6}  {:>10}  {:>20}", "ID", "SIZE", "CREATED (unix ms)");
            for p in pins {
                let size = format!("{}x{}", p.width, p.height);
                println!("{:>6}  {:>10}  {:>20}", p.id, size, p.created_at_unix_ms);
            }
        }
        IpcResponse::Error(err) => {
            eprintln!("error: {err}");
        }
    }
}
