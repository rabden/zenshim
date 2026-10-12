use std::io::Write;
use std::process::{Command, Stdio};

/// Copy via the first clipboard tool that works (pbcopy, wl-copy,
/// xclip, xsel, clip.exe).
pub fn copy(text: &str) -> bool {
    let attempts: [(&str, &[&str]); 5] = [
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("clip.exe", &[]),
    ];
    for (cmd, args) in attempts {
        if let Ok(mut child) = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            // Take stdin so it closes after the write; the tool reads to EOF.
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok_and(|s| s.success()) {
                return true;
            }
        }
    }
    false
}
