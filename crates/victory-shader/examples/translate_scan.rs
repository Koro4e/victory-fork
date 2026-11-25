//! Translates every dumped compute program and counts what passes, with
//! the per-program reason when it does not: the state of the whole dump
//! after a change to the translator.
//! Usage: `translate_scan <dir with .cdxil files>`
use victory_shader::{Container, FourCC};

fn main() {
    let dir = std::env::args().nth(1).expect("usage: translate_scan <dir>");
    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut reasons: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("dir") {
        let path = entry.expect("entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
        if !name.ends_with(".compute.cdxil") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        match victory_shader::emit::translate(&bytes) {
            Ok(_) => {
                ok += 1;
            }
            Err(e) => {
                failed += 1;
                let full = e.to_string();
                let tail = full.split(':').last().unwrap_or("").trim().to_owned();
                let key = if tail.len() > 60 { tail[..60].to_owned() } else { tail };
                *reasons.entry(key).or_insert(0) += 1;
                if full.contains("lane mask exchanged") {
                    names.push(name);
                }
            }
        }
    }
    println!("ok {ok}, failed {failed}");
    let mut reasons: Vec<(String, usize)> = reasons.into_iter().collect();
    reasons.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (why, n) in reasons.iter().take(8) {
        println!("  {n:4} {why}");
    }
    if !names.is_empty() {
        println!("lane-mask refusals left: {}", names.len());
        for n in names.iter().take(5) {
            println!("  {n}");
        }
    }
}
