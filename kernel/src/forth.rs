use core::iter::Peekable;

use alloc::{collections::BTreeMap, format, string::String, string::ToString, vec::Vec};
use lazy_static::lazy_static;
use spin::Mutex;

use crate::{console, editor, fs};

#[derive(Clone)]
enum StackValue {
    Int(i64),
    Str(String),
}

#[derive(Clone)]
enum Token {
    Word(String),
    Number(i64),
    StringLiteral(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ForthStatus {
    Continue,
    Exit,
}

struct PendingDefinition {
    name: String,
    body: Vec<Token>,
}

pub struct ForthInterpreter {
    stack: Vec<StackValue>,
    user_words: BTreeMap<String, Vec<Token>>,
    pending_def: Option<PendingDefinition>,
    quiet: bool,
}

lazy_static! {
    static ref FORTH: Mutex<Option<ForthInterpreter>> = Mutex::new(None);
}

const BUILTINS: &[&str] = &[
    "+", "-", "*", "/", "mod", "dup", "drop", "swap", "over", ".s", ".", "emit", "cr", "depth",
    "clearstack", "words", "load", "ls", "cat", "touch", "rm", "write-file", "append-line",
    "type", "vight", "nano", "edit", "bye", "exit", "quit",
];

const EXAMPLES: &[(&str, &str)] = &[
    ("Hello.f", ".\" Hello, Forth world!\" cr"),
    (
        "Math.f",
        ": square dup * ;\n: cube dup dup * * ;\n3 square . cr 4 cube . cr",
    ),
    (
        "Stack.f",
        "1 2 3 .s cr swap .s cr drop .s cr clearstack .s cr",
    ),
    (
        "FileIO.f",
        ".\" Writing sample...\" cr \"sample.txt\" touch \"sample.txt\" \"This came from Forth.\" write-file \"sample.txt\" cat",
    ),
];

pub fn install_examples() {
    console::write_line("Added:");
    for (name, body) in EXAMPLES {
        let _ = fs::write_file(name, body);
        console::write_line(&format!("  {}", name));
    }
}

pub fn is_active() -> bool {
    FORTH.lock().is_some()
}

pub fn start_repl() {
    let mut guard = FORTH.lock();
    if guard.is_some() {
        console::write_line("Forth is already running. Type 'bye' to leave.");
        return;
    }

    let interpreter = ForthInterpreter::new(false);
    console::write_line("Forth mode. Type 'bye' to return to the shell.");
    console::write_line("Strings use quotes (e.g., \"hi\" .). Definitions use : name ... ;");

    *guard = Some(interpreter);
}

pub fn run_file_once(path: &str) {
    let mut interpreter = ForthInterpreter::new(true);
    if let Err(e) = interpreter.load_script(path) {
        console::write_line(e);
    }
}

pub fn handle_input(line: &str) -> bool {
    let mut guard = FORTH.lock();
    let Some(interpreter) = guard.as_mut() else {
        return false;
    };

    match interpreter.process_line(line) {
        Ok(ForthStatus::Continue) => {}
        Ok(ForthStatus::Exit) => {
            console::write_line("Leaving Forth.");
            *guard = None;
        }
        Err(e) => console::write_line(e),
    }
    true
}

impl ForthInterpreter {
    fn new(quiet: bool) -> Self {
        Self {
            stack: Vec::new(),
            user_words: BTreeMap::new(),
            pending_def: None,
            quiet,
        }
    }

    fn process_line(&mut self, line: &str) -> Result<ForthStatus, &'static str> {
        let tokens = tokenize(line);
        self.eval_tokens(tokens)
    }

    fn eval_tokens(&mut self, tokens: Vec<Token>) -> Result<ForthStatus, &'static str> {
        let mut iter = tokens.into_iter().peekable();

        while let Some(token) = iter.next() {
            if self.pending_def.is_some() {
                if let Token::Word(ref w) = token {
                    if w == ";" {
                        let def = self.pending_def.take().unwrap();
                        self.user_words.insert(def.name.clone(), def.body);
                        if !self.quiet {
                            console::write_line(&format!("ok: defined {}", def.name));
                        }
                        continue;
                    }
                }
                if let Some(def) = self.pending_def.as_mut() {
                    def.body.push(token);
                }
                continue;
            }

            if let Token::Word(ref w) = token {
                if w == ":" {
                    let Some(next_token) = iter.next() else {
                        return Err("Expected a word name after ':'");
                    };
                    let name = match next_token {
                        Token::Word(n) => n.to_ascii_lowercase(),
                        _ => return Err("Word names must be text."),
                    };
                    self.pending_def = Some(PendingDefinition { name, body: Vec::new() });
                    continue;
                }
                if w == ";" {
                    console::write_line("';' without matching ':'");
                    continue;
                }
            }

            let status = self.execute_token(token, &mut iter)?;
            if status == ForthStatus::Exit {
                return Ok(ForthStatus::Exit);
            }
        }

        Ok(ForthStatus::Continue)
    }

    fn execute_token<I>(
        &mut self,
        token: Token,
        iter: &mut Peekable<I>,
    ) -> Result<ForthStatus, &'static str>
    where
        I: Iterator<Item = Token>,
    {
        match token {
            Token::Number(n) => {
                self.stack.push(StackValue::Int(n));
                Ok(ForthStatus::Continue)
            }
            Token::StringLiteral(s) => {
                self.stack.push(StackValue::Str(s));
                Ok(ForthStatus::Continue)
            }
            Token::Word(w) => self.execute_word(&w, iter),
        }
    }

    fn execute_word<I>(
        &mut self,
        word: &str,
        iter: &mut Peekable<I>,
    ) -> Result<ForthStatus, &'static str>
    where
        I: Iterator<Item = Token>,
    {
        if let Some(status) = self.run_builtin(word, iter)? {
            return Ok(status);
        }

        if let Some(body) = self.user_words.get(&word.to_ascii_lowercase()).cloned() {
            let status = self.eval_tokens(body)?;
            return Ok(status);
        }

        if let Some(num) = parse_number(word) {
            self.stack.push(StackValue::Int(num));
            return Ok(ForthStatus::Continue);
        }

        let msg = format!("Unknown word: {}", word);
        console::write_line(&msg);
        Ok(ForthStatus::Continue)
    }

    fn run_builtin<I>(
        &mut self,
        word: &str,
        iter: &mut Peekable<I>,
    ) -> Result<Option<ForthStatus>, &'static str>
    where
        I: Iterator<Item = Token>,
    {
        let w = word.to_ascii_lowercase();
        match w.as_str() {
            "bye" | "exit" | "quit" => return Ok(Some(ForthStatus::Exit)),
            "." => {
                let v = self.pop_any()?;
                self.print_value(&v);
                return Ok(Some(ForthStatus::Continue));
            }
            ".s" => {
                self.print_stack();
                return Ok(Some(ForthStatus::Continue));
            }
            "dup" => {
                let v = self.stack.last().cloned().ok_or("Stack underflow")?;
                self.stack.push(v);
                return Ok(Some(ForthStatus::Continue));
            }
            "drop" => {
                self.stack.pop().ok_or("Stack underflow")?;
                return Ok(Some(ForthStatus::Continue));
            }
            "swap" => {
                let len = self.stack.len();
                if len < 2 {
                    return Err("Stack underflow");
                }
                self.stack.swap(len - 1, len - 2);
                return Ok(Some(ForthStatus::Continue));
            }
            "over" => {
                let len = self.stack.len();
                if len < 2 {
                    return Err("Stack underflow");
                }
                let v = self.stack[len - 2].clone();
                self.stack.push(v);
                return Ok(Some(ForthStatus::Continue));
            }
            "depth" => {
                self.stack.push(StackValue::Int(self.stack.len() as i64));
                return Ok(Some(ForthStatus::Continue));
            }
            "clearstack" => {
                self.stack.clear();
                console::write_line("Stack cleared.");
                return Ok(Some(ForthStatus::Continue));
            }
            "cr" => {
                console::write_line("");
                return Ok(Some(ForthStatus::Continue));
            }
            "emit" => {
                let v = self.pop_int()?;
                let ch = core::char::from_u32(v as u32).unwrap_or('?');
                console::write(&ch.to_string());
                return Ok(Some(ForthStatus::Continue));
            }
            ".\"" => {
                if let Some(text) = self.take_text_argument(iter) {
                    
                    console::write(&text);
                } else {
                    console::write_line(".\" expects text after the quote.");
                }
                return Ok(Some(ForthStatus::Continue));
            }
            "+" => {
                self.binary_op(|a, b| a + b)?;
                return Ok(Some(ForthStatus::Continue));
            }
            "-" => {
                self.binary_op(|a, b| a - b)?;
                return Ok(Some(ForthStatus::Continue));
            }
            "*" => {
                self.binary_op(|a, b| a * b)?;
                return Ok(Some(ForthStatus::Continue));
            }
            "/" => {
                let b = self.pop_int()?;
                if b == 0 {
                    return Err("Divide by zero");
                }
                let a = self.pop_int()?;
                self.stack.push(StackValue::Int(a / b));
                return Ok(Some(ForthStatus::Continue));
            }
            "mod" => {
                let b = self.pop_int()?;
                if b == 0 {
                    return Err("Divide by zero");
                }
                let a = self.pop_int()?;
                self.stack.push(StackValue::Int(a % b));
                return Ok(Some(ForthStatus::Continue));
            }
            "words" => {
                self.list_words();
                return Ok(Some(ForthStatus::Continue));
            }
            "ls" => {
                print_listing();
                return Ok(Some(ForthStatus::Continue));
            }
            "cat" => {
                let Some(name) = self.take_text_argument(iter) else {
                    console::write_line("cat expects a path.");
                    return Ok(Some(ForthStatus::Continue));
                };
                print_file(&name);
                return Ok(Some(ForthStatus::Continue));
            }
            "touch" => {
                let Some(name) = self.take_text_argument(iter) else {
                    console::write_line("touch expects a path.");
                    return Ok(Some(ForthStatus::Continue));
                };
                match fs::touch(&name) {
                    Ok(_) => console::write_line(&format!("File ready: {}", name)),
                    Err("File already exists.") => console::write_line("File already exists."),
                    Err(e) => console::write_line(e),
                }
                return Ok(Some(ForthStatus::Continue));
            }
            "write-file" => {
                let Some(name) = self.take_text_argument(iter) else {
                    console::write_line("write-file expects <name> <text>.");
                    return Ok(Some(ForthStatus::Continue));
                };
                let Some(text) = self.take_text_argument(iter) else {
                    console::write_line("write-file expects <name> <text>.");
                    return Ok(Some(ForthStatus::Continue));
                };
                match fs::write_file(&name, &text) {
                    Ok(_) => console::write_line(&format!("Wrote {}", name)),
                    Err(e) => console::write_line(e),
                }
                return Ok(Some(ForthStatus::Continue));
            }
            "append-line" => {
                let Some(name) = self.take_text_argument(iter) else {
                    console::write_line("append-line expects <name> <text>.");
                    return Ok(Some(ForthStatus::Continue));
                };
                let Some(text) = self.take_text_argument(iter) else {
                    console::write_line("append-line expects <name> <text>.");
                    return Ok(Some(ForthStatus::Continue));
                };
                match fs::append_line(&name, &text) {
                    Ok(_) => console::write_line(&format!("Appended to {}", name)),
                    Err(e) => console::write_line(e),
                }
                return Ok(Some(ForthStatus::Continue));
            }
            "rm" => {
                let Some(name) = self.take_text_argument(iter) else {
                    console::write_line("rm expects a path.");
                    return Ok(Some(ForthStatus::Continue));
                };
                match fs::delete_file(&name) {
                    Ok(_) => console::write_line(&format!("Deleted {}", name)),
                    Err(e) => console::write_line(e),
                }
                return Ok(Some(ForthStatus::Continue));
            }
            "load" => {
                let Some(name) = self.take_text_argument(iter) else {
                    console::write_line("load expects a file name.");
                    return Ok(Some(ForthStatus::Continue));
                };
                let status = self.load_script(&name)?;
                return Ok(Some(status));
            }
            "type" => {
                let v = self.pop_any()?;
                self.print_value_inline(&v);
                return Ok(Some(ForthStatus::Continue));
            }
            "vight" | "nano" | "edit" => {
                if let Some(name) = self.take_text_argument(iter) {
                    editor::start(&name);
                } else {
                    console::write_line("vight expects a file name.");
                }
                return Ok(Some(ForthStatus::Continue));
            }
            _ => {}
        }
        Ok(None)
    }

    fn list_words(&self) {
        let mut words: Vec<String> = BUILTINS.iter().map(|s| s.to_string()).collect();
        for k in self.user_words.keys() {
            words.push(k.clone());
        }
        words.sort_unstable();

        console::write_line("Defined words:");
        for chunk in words.chunks(8) {
            let mut line = String::new();
            for (i, w) in chunk.iter().enumerate() {
                if i > 0 {
                    line.push(' ');
                }
                line.push_str(w);
            }
            console::write_line(&line);
        }
    }

    fn pop_int(&mut self) -> Result<i64, &'static str> {
        match self.stack.last() {
            Some(StackValue::Int(v)) => {
                let val = *v;
                self.stack.pop();
                Ok(val)
            }
            Some(StackValue::Str(_)) => Err("Expected number on stack"),
            None => Err("Stack underflow"),
        }
    }

    fn pop_any(&mut self) -> Result<StackValue, &'static str> {
        self.stack.pop().ok_or("Stack underflow")
    }

    fn binary_op<F>(&mut self, op: F) -> Result<(), &'static str>
    where
        F: FnOnce(i64, i64) -> i64,
    {
        let b = self.pop_int()?;
        let a = self.pop_int()?;
        self.stack.push(StackValue::Int(op(a, b)));
        Ok(())
    }

    fn print_stack(&self) {
        if self.stack.is_empty() {
            console::write_line("<empty stack>");
            return;
        }

        let mut parts: Vec<String> = Vec::new();
        for v in &self.stack {
            parts.push(self.value_string(v));
        }
        let line = parts.join(" ");
        console::write_line(&format!("<{}> {}", self.stack.len(), line));
    }

    fn value_string(&self, v: &StackValue) -> String {
        match v {
            StackValue::Int(n) => format!("{}", n),
            StackValue::Str(s) => format!("\"{}\"", s),
        }
    }

    fn print_value(&self, v: &StackValue) {
        match v {
            StackValue::Int(n) => console::write_line(&format!("{}", n)),
            StackValue::Str(s) => console::write_line(s),
        }
    }

    fn print_value_inline(&self, v: &StackValue) {
        match v {
            StackValue::Int(n) => console::write(&format!("{}", n)),
            StackValue::Str(s) => console::write(s),
        }
    }

    fn take_text_argument<I>(&mut self, iter: &mut Peekable<I>) -> Option<String>
    where
        I: Iterator<Item = Token>,
    {
        if let Some(StackValue::Str(s)) = self.stack.last() {
            let s = s.clone();
            self.stack.pop();
            return Some(s);
        }

        if let Some(tok) = iter.next() {
            return match tok {
                Token::StringLiteral(s) => Some(s),
                Token::Word(w) => Some(w),
                Token::Number(n) => Some(n.to_string()),
            };
        }
        None
    }

    fn load_script(&mut self, name: &str) -> Result<ForthStatus, &'static str> {
        match fs::read_file(name) {
            Some(body) => {
                for line in body.lines() {
                    let status = self.process_line(line)?;
                    if status == ForthStatus::Exit {
                        return Ok(ForthStatus::Exit);
                    }
                }
                Ok(ForthStatus::Continue)
            }
            None => {
                console::write_line("File not found.");
                Ok(ForthStatus::Continue)
            }
        }
    }
}

fn tokenize(line: &str) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    let mut current = String::new();
    let mut in_string = false;

    for ch in line.chars() {
        if in_string {
            if ch == '"' {
                tokens.push(Token::StringLiteral(current.clone()));
                current.clear();
                in_string = false;
            } else {
                let _ = current.push(ch);
            }
            continue;
        }

        if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(make_token(&current));
                current.clear();
            }
        } else if ch == '"' {
            if current == "." {
                tokens.push(Token::Word(".\"".to_string()));
                current.clear();
            } else if !current.is_empty() {
                tokens.push(make_token(&current));
                current.clear();
            }
            if !current.is_empty() {
                tokens.push(make_token(&current));
                current.clear();
            }
            in_string = true;
        } else {
            let _ = current.push(ch);
        }
    }

    if in_string {
        tokens.push(Token::StringLiteral(current));
    } else if !current.is_empty() {
        tokens.push(make_token(&current));
    }

    tokens
}

fn make_token(raw: &str) -> Token {
    if let Some(n) = parse_number(raw) {
        Token::Number(n)
    } else {
        Token::Word(raw.to_string())
    }
}

fn parse_number(word: &str) -> Option<i64> {
    if let Some(stripped) = word.strip_prefix("0x") {
        i64::from_str_radix(stripped, 16).ok()
    } else {
        word.parse::<i64>().ok()
    }
}

fn print_listing() {
    let entries = fs::list_files();
    if entries.is_empty() {
        if fs::current_dir() == "\\" {
            console::write_line("(filesystem empty)");
        } else {
            console::write_line("(directory empty)");
        }
        return;
    }

    for e in entries {
        if e.is_dir {
            console::write_line(&format!("{:>6}  {}\\", "<DIR>", e.name));
        } else {
            console::write_line(&format!("{:>6} bytes  {}", e.size, e.name));
        }
    }
}

fn print_file(name: &str) {
    match fs::read_file(name) {
        Some(body) => {
            if body.is_empty() {
                console::write_line("(empty file)");
            } else {
                for line in body.lines() {
                    console::write_line(line);
                }
            }
        }
        None => console::write_line("File not found."),
    }
}
