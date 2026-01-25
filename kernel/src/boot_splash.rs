#![allow(dead_code)]

use crate::console::{with_console, DrawPos, size_chars};
use crate::wait;
use alloc::format;

pub fn show() {
    const ART: &[&str] = &[
        r"    _          _                       _        ",
        r"   / \   __  _(_) ___  _ __ ___   __ _| |_ __ _ ",
        r"  / _ \  \ \/ / |/ _ \| '_ ` _ \ / _` | __/ _` |",
        r" / ___ \  >  <| | (_) | | | | | | (_| | || (_| |",
        r"/_/   \_\/_/\_\_|\___/|_| |_| |_|\__,_|\__\__,_|",
        "",
        "",
    ];

    const STATUS_FRAMES: &[&str] = &[
        "booting Axiomata.",
        "booting Axiomata.",
        "booting Axiomata..",
        "booting Axiomata...",
    ];

    let art_width = ART.iter().map(|l| l.len()).max().unwrap_or(0);
    let status_width = STATUS_FRAMES.iter().map(|l| l.len()).max().unwrap_or(0);
    let block_width = core::cmp::max(art_width, status_width);
    let block_height = ART.len();

    let (cols, rows) = size_chars();
    let start_x = cols.saturating_sub(block_width) / 2;
    let start_y = rows.saturating_sub(block_height) / 2;
    let status_row = start_y + block_height.saturating_sub(1);

    with_console(|c| {
        c.clear();
        for (i, line) in ART.iter().enumerate() {
            let padded = format!("{:<width$}", *line, width = block_width);
            c.draw_text_at_char(DrawPos::Char(start_x, start_y + i), &padded);
        }
    });

    for i in 0..8 {
        let msg = STATUS_FRAMES[i % STATUS_FRAMES.len()];
        let padded = format!("{:<width$}", msg, width = block_width);
        with_console(|c| {
            c.draw_text_at_char(DrawPos::Char(start_x, status_row), &padded);
        });
        wait::bms(400);
    }

    wait::bms(600);
    with_console(|c| c.clear());
}
