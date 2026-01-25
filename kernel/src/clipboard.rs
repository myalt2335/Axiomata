use spin::Mutex;
use alloc::string::String;
use lazy_static::lazy_static;

lazy_static! {
    static ref CLIPBOARD: Mutex<String> = Mutex::new(String::new());
}

pub fn set_text(text: &str) {
    let mut clip = CLIPBOARD.lock();
    *clip = String::from(text);
}

pub fn get_text() -> String {
    let clip = CLIPBOARD.lock();
    clip.clone()
}
