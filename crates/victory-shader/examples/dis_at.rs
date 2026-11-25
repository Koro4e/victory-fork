//! Decodes a dumped native program and prints the instructions around an
//! offset: what the translator refuses, in context.
//! Usage: `dis_at <container.cdxil> <offset-hex>`
use victory_shader::{Container, FourCC, gfx10};

fn main() {
    let path = std::env::args().nth(1).expect("container");
    let at = usize::from_str_radix(
        std::env::args().nth(2).expect("offset").trim_start_matches("0x"),
        16,
    )
    .expect("offset");
    let bytes = std::fs::read(&path).expect("read");
    let c = Container::parse(&bytes).expect("container");
    let part = c.part(FourCC::DXIL).expect("dxil part");
    let programs = victory_shader::native_programs(part.data).expect("programs");
    for p in programs.iter() {
        let code = &p.code;
        if at >= code.len() {
            continue;
        }
        let instrs = gfx10::decode(code).expect("decode");
        println!(
            "== program of {} bytes, {} instructions, asking for {:#x}",
            code.len(),
            instrs.len(),
            at
        );
        for i in instrs.iter() {
            if i.offset + 16 < at.saturating_sub(64) || i.offset > at + 64 {
                continue;
            }
            let mark = if i.offset <= at && at < i.offset + 4 * i.words.len() { "=>" } else { "  " };
            println!(
                "{mark} {:#06x}: {} ({}, {} dwords)",
                i.offset,
                i.name().unwrap_or("??"),
                format!("{:?}", i.encoding),
                i.words.len()
            );
        }
    }
}
