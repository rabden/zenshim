use std::io::{Write, BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;

use tokio::sync::watch;

use crate::tunnel::TunnelUrl;

pub struct ConsoleState {
    pub base_url: String,
    pub gateway_key: String,
    pub models: Vec<String>,
    pub tunnel: TunnelUrl,
}

fn copy_to_clipboard(text: &str) -> bool {
    let pbcopy: (&str, Vec<&str>) = ("pbcopy", vec![]);
    let wl_copy: (&str, Vec<&str>) = ("wl-copy", vec![]);
    let xclip: (&str, Vec<&str>) = ("xclip", vec!["-selection", "clipboard"]);
    let xsel: (&str, Vec<&str>) = ("xsel", vec!["--clipboard", "--input"]);
    let attempts = [pbcopy, wl_copy, xclip, xsel];
    for (cmd, args) in attempts {
        if let Ok(mut child) = Command::new(cmd)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok_and(|s| s.success()) {
                return true;
            }
        }
    }
    false
}

fn emit(text: &str) {
    println!("{text}");
    print!("zenshim> ");
    let _ = std::io::stdout().flush();
}

fn show_summary(state: &ConsoleState) {
    let tunnel = state.tunnel.lock().unwrap().clone();
    println!();
    println!("  zenshim — OpenAI-compatible gateway for OpenCode Zen's free tier");
    println!("  base url : {}", state.base_url);
    match &tunnel {
        Some(url) => println!("  tunnel   : {url} (public)"),
        None => println!("  tunnel   : none (localhost only)"),
    }
    println!("  api key  : hidden — 'k' copies it, 'K' reveals it");
    println!("  models   : {}", state.models.join(", "));
    println!();
    println!("  k copy api key | m pick model id | u copy base url | t copy tunnel url");
    println!("  K reveal key  | s summary        | q quit");
    print!("zenshim> ");
    let _ = std::io::stdout().flush();
}

/// Interactive console loop on a dedicated thread. Runs only when stdin
/// is a TTY; commands copy values to the clipboard instead of printing
/// them so secrets stay off the screen and scrollback.
pub fn spawn(state: ConsoleState, shutdown: watch::Sender<bool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let ConsoleState {
            base_url,
            gateway_key,
            models,
            tunnel,
        } = state;
        let state = ConsoleState {
            base_url,
            gateway_key,
            models,
            tunnel,
        };
        show_summary(&state);
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return, // stdin closed — server keeps running
                Ok(_) => {}
            }
            let cmd = line.trim();
            match cmd {
                "k" | "key" => {
                    if copy_to_clipboard(&state.gateway_key) {
                        emit("api key copied to clipboard");
                    } else {
                        emit("no clipboard found — api key: (use 'K' to reveal)");
                    }
                }
                "K" | "reveal" => {
                    emit(&format!("api key: {}", state.gateway_key));
                }
                "m" | "models" => {
                    if state.models.is_empty() {
                        emit("no models served");
                        continue;
                    }
                    println!("  pick a model to copy:");
                    for (index, id) in state.models.iter().enumerate() {
                        println!("  {index}. {id}");
                    }
                    println!("  a. copy all (comma-separated)");
                    print!("zenshim> ");
                    let _ = std::io::stdout().flush();
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let pick = line.trim();
                    let value = if pick == "a" || pick == "all" {
                        state.models.join(",")
                    } else if let Ok(index) = pick.parse::<usize>() {
                        match state.models.get(index) {
                            Some(id) => id.clone(),
                            None => {
                                emit("no such model");
                                continue;
                            }
                        }
                    } else {
                        emit("pick a number or 'a'");
                        continue;
                    };
                    if copy_to_clipboard(&value) {
                        emit(&format!("copied: {value}"));
                    } else {
                        emit(&format!("no clipboard — model id: {value}"));
                    }
                }
                "u" | "url" => {
                    if copy_to_clipboard(&state.base_url) {
                        emit("base url copied to clipboard");
                    } else {
                        emit(&format!("no clipboard — base url: {}", state.base_url));
                    }
                }
                "t" | "tunnel" => {
                    let url = state.tunnel.lock().unwrap().clone();
                    match url {
                        Some(url) => {
                            if copy_to_clipboard(&url) {
                                emit("tunnel url copied to clipboard");
                            } else {
                                emit(&format!("no clipboard — tunnel url: {url}"));
                            }
                        }
                        None => emit("no tunnel active (start with --tunnel)"),
                    }
                }
                "s" | "summary" => show_summary(&state),
                "q" | "quit" | "exit" => {
                    let _ = shutdown.send(true);
                    emit("shutting down…");
                    return;
                }
                "" => {
                    print!("zenshim> ");
                    let _ = std::io::stdout().flush();
                }
                other => emit(&format!(
                    "unknown command '{other}' — k key, m models, u url, t tunnel, s summary, q quit"
                )),
            }
        }
    })
}

/// Non-interactive summary for pipes/scripts/TTY-less environments.
pub fn print_plain(state: &ConsoleState) {
    let tunnel = state.tunnel.lock().unwrap().clone();
    println!("base url : {}", state.base_url);
    match &tunnel {
        Some(url) => println!("tunnel   : {url} (public)"),
        None => {}
    }
    println!("api key  : hidden — saved in the config; pass --gateway-key to override");
    println!("models   : {}", state.models.join(", "));
}

