//! Decodes a program from the pipeline library: coverage report.
//! `cargo run --example prog_decode -- <offset> <size> <path-to-lib>`

fn main() {
    let mut args = std::env::args().skip(1);
    let off: usize = args.next().unwrap().parse().unwrap();
    let size: usize = args.next().unwrap().parse().unwrap();
    let path = args.next().unwrap();
    let lib = std::fs::read(&path).unwrap();
    let code = &lib[off..off + size];
    match victory_shader::gfx10::decode(code) {
        Ok(insts) => {
            let covered: usize = insts.iter().map(|i| i.words.len() * 4).sum();
            let named = insts.iter().filter(|i| i.name().is_some()).count();
            println!(
                "{} bytes: {} instructions, {} bytes reachable ({:.1}%), {} named",
                size,
                insts.len(),
                covered,
                100.0 * covered as f64 / size as f64,
                named
            );
            let last = insts.last().unwrap();
            println!("last: {} at +{:x}", last.name().unwrap_or("?"), last.offset);
        }
        Err(e) => println!("decode error: {e}"),
    }
}
