use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lazy_static::lazy_static;
use spin::Mutex;

use crate::{console, fs};

struct EditorSession {
    filename: String,
    buffer: Vec<String>,
    dirty: bool,
}

lazy_static! {
    static ref EDITOR: Mutex<Option<EditorSession>> = Mutex::new(None);
}

pub fn is_active() -> bool {
    EDITOR.lock().is_some()
}

pub fn start(filename: &str) {
    if is_active() {
        console::write_line("Close the current editor session before opening another.");
        return;
    }

    let canonical = match fs::ensure_file(filename) {
        Ok(name) => name,
        Err(e) => {
            console::write_line(e);
            return;
        }
    };

    let existing = fs::read_file(&canonical).unwrap_or_default();
    let mut buffer = Vec::new();
    for line in existing.lines() {
        buffer.push(String::from(line));
    }

    {
        let mut guard = EDITOR.lock();
        *guard = Some(EditorSession {
            filename: canonical.clone(),
            buffer,
            dirty: false,
        });
    }

    console::write_line("");
    console::write_line(&format!("Vight: editing {}", canonical));
    console::write_line("Type text to append lines.");
    console::write_line("Commands start with ':' (try :help).");
    console::write_line("------------------------------------------------------------");
    show_buffer_snapshot();
    show_status();
}

pub fn handle_input(input: &str) -> bool {
    let mut guard = EDITOR.lock();
    let Some(session) = guard.as_mut() else {
        return false;
    };

    let action = if input.trim_start().starts_with(':') {
        handle_command_line(session, input.trim_start().trim_start_matches(':'))
    } else {
        session.buffer.push(String::from(input));
        session.dirty = true;
        console::write_line(&format!("{} | {}", session.buffer.len(), input));
        EditorAction::Stay
    };

    if matches!(action, EditorAction::Close) {
        *guard = None;
    }
    true
}

enum EditorAction {
    Stay,
    Close,
}

fn handle_command_line(session: &mut EditorSession, line: &str) -> EditorAction {
    let mut parts = line.splitn(2, ' ');
    let command = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();

    match command {
        "wq" => {
            if save(session, Some(rest), true) {
                console::write_line("Saved and closed editor.");
                return EditorAction::Close;
            }
        }
        "w" | "write" => {
            save(session, Some(rest), false);
        }
        "q" | "quit" => {
            if session.dirty {
                console::write_line("Unsaved changes. Use :w to save or :q! to quit anyway.");
            } else {
                console::write_line("Closed editor.");
                return EditorAction::Close;
            }
        }
        "q!" | "quit!" => {
            console::write_line("Closed editor (unsaved changes discarded).");
            return EditorAction::Close;
        }
        "p" | "print" => show_buffer(session),
        "set" => set_line(session, rest),
        "i" | "insert" => insert_line(session, rest),
        "d" | "del" | "delete" => delete_line(session, rest),
        "clear" => {
            session.buffer.clear();
            session.dirty = true;
            console::write_line("Buffer cleared.");
            print_status_line(session);
        }
        "find" | "search" => find_in_buffer(session, rest),
        "status" | "info" => print_status_line(session),
        "reload" => reload_file(session, false),
        "reload!" => reload_file(session, true),
        "help" => print_help(),
        "" => {}
        _ => console::write_line("Unknown editor command. Use :help for a list."),
    }
    EditorAction::Stay
}

fn save(session: &mut EditorSession, target: Option<&str>, quiet: bool) -> bool {
    let next_name = target
        .and_then(|t| {
            let trimmed = t.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
        .unwrap_or(&session.filename);

    let canonical = match fs::canonical_name(next_name) {
        Ok(name) => name,
        Err(e) => {
            console::write_line(e);
            return false;
        }
    };

    let mut body = String::new();
    for (i, line) in session.buffer.iter().enumerate() {
        if i > 0 {
            body.push('\n');
        }
        body.push_str(line);
    }

    match fs::write_file(next_name, &body) {
        Ok(_) => {
            session.filename = canonical;
            session.dirty = false;
            if !quiet {
                console::write_line(&format!("Saved {}", session.filename));
                print_status_line(session);
            }
            true
        }
        Err(e) => {
            console::write_line(e);
            false
        }
    }
}

fn set_line(session: &mut EditorSession, args: &str) {
    let (idx, text) = split_line_arg(args);
    let Some(idx) = idx else {
        console::write_line("Usage: :set <line-number> <text>");
        return;
    };
    if idx == 0 || idx > session.buffer.len() {
        console::write_line("Line number out of range.");
        return;
    }
    let pos = idx - 1;
    session.buffer[pos] = String::from(text);
    session.dirty = true;
    console::write_line(&format!("Updated line {}", idx));
    print_status_line(session);
}

fn insert_line(session: &mut EditorSession, args: &str) {
    let (idx, text) = split_line_arg(args);
    let Some(idx) = idx else {
        console::write_line("Usage: :insert <line-number> <text>");
        return;
    };

    let pos = idx.saturating_sub(1);
    if pos > session.buffer.len() {
        console::write_line("Line number out of range.");
        return;
    }

    session.buffer.insert(pos, String::from(text));
    session.dirty = true;
    console::write_line(&format!("Inserted at line {}", idx));
    print_status_line(session);
}

fn delete_line(session: &mut EditorSession, args: &str) {
    let idx = args.split_whitespace().next().and_then(|n| n.parse::<usize>().ok());
    let Some(idx) = idx else {
        console::write_line("Usage: :delete <line-number>");
        return;
    };
    if idx == 0 || idx > session.buffer.len() {
        console::write_line("Line number out of range.");
        return;
    }
    session.buffer.remove(idx - 1);
    session.dirty = true;
    console::write_line(&format!("Deleted line {}", idx));
    print_status_line(session);
}

fn show_buffer(session: &EditorSession) {
    if session.buffer.is_empty() {
        console::write_line("(empty buffer)");
        return;
    }

    for (i, line) in session.buffer.iter().enumerate() {
        console::write_line(&format!("{:>4}: {}", i + 1, line));
        if i > 200 {
            console::write_line("... (output truncated)");
            break;
        }
    }
}

fn show_buffer_snapshot() {
    let guard = EDITOR.lock();
    let Some(session) = guard.as_ref() else { return; };
    if session.buffer.is_empty() {
        console::write_line("(new file)");
    } else {
        console::write_line("Current contents:");
        show_buffer(session);
    }
}

fn split_line_arg(args: &str) -> (Option<usize>, &str) {
    let mut parts = args.splitn(2, ' ');
    let idx = parts.next().and_then(|n| n.parse::<usize>().ok());
    let text = parts.next().unwrap_or("");
    (idx, text)
}

fn reload_file(session: &mut EditorSession, force: bool) {
    if session.dirty && !force {
        console::write_line("Unsaved changes would be lost. Use :reload! to discard them.");
        return;
    }

    match fs::read_file(&session.filename) {
        Some(body) => {
            session.buffer.clear();
            for line in body.lines() {
                session.buffer.push(String::from(line));
            }
            session.dirty = false;
            console::write_line(&format!("Reloaded {}", session.filename));
            print_status_line(session);
        }
        None => console::write_line("File missing; nothing reloaded."),
    }
}

fn find_in_buffer(session: &EditorSession, needle: &str) {
    if needle.trim().is_empty() {
        console::write_line("Usage: :find <text>");
        return;
    }

    let query = needle.to_ascii_lowercase();
    let mut hits = 0usize;
    let mut shown = 0usize;
    let max_show = 40usize;

    for (i, line) in session.buffer.iter().enumerate() {
        if line.to_ascii_lowercase().contains(&query) {
            hits += 1;
            if shown < max_show {
                console::write_line(&format!("{:>4}: {}", i + 1, line));
                shown += 1;
            }
        }
    }

    if hits == 0 {
        console::write_line("No matches found.");
        return;
    }

    if hits > shown {
        console::write_line(&format!("... {} more match(es) not shown", hits - shown));
    }
    console::write_line(&format!("{} match(es).", hits));
}

fn print_status_line(session: &EditorSession) {
    let state = if session.dirty { "unsaved" } else { "saved" };
    console::write_line(&format!(
        "{} | {} line(s) | {}",
        session.filename,
        session.buffer.len(),
        state
    ));
}

fn show_status() {
    let guard = EDITOR.lock();
    let Some(session) = guard.as_ref() else { return; };
    print_status_line(session);
}

fn print_help() {
    console::write_line("Vight commands:");
    console::write_line("  :w [name]   - save (optionally save-as)");
    console::write_line("  :wq         - save and quit");
    console::write_line("  :q          - quit (warns if unsaved)");
    console::write_line("  :p          - print buffer with line numbers");
    console::write_line("  :set N X    - replace line N with text X");
    console::write_line("  :insert N X - insert text X before line N");
    console::write_line("  :delete N   - delete line N");
    console::write_line("  :find <text>  - search lines containing text");
    console::write_line("  :status     - show file, line count, dirty state");
    console::write_line("  :reload     - reload file (use :reload! to discard changes)");
    console::write_line("  :clear      - clear buffer");
}
