//! Instruction mix of a dumped native compute program: what the shader
//! spends its instructions on. Usage: `opmix <container.cdxil> ...`
use victory_shader::{Container, FourCC};

fn main() {
    for path in std::env::args().skip(1) {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{path}: {e}");
                continue;
            }
        };
        let Ok(c) = Container::parse(&bytes) else {
            eprintln!("{path}: not a container");
            continue;
        };
        let Some(part) = c.part(FourCC(*b"DXIL")) else {
            eprintln!("{path}: no DXIL part");
            continue;
        };
        let Ok(programs) = victory_shader::native_programs(part.data) else {
            eprintln!("{path}: no native programs");
            continue;
        };
        for p in &programs {
            let Ok(code) = victory_shader::gfx10::decode(&p.code) else {
                eprintln!("{path}: cannot decode");
                continue;
            };
            let mut mix: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
            let (mut branches, mut lds, mut mem) = (0usize, 0usize, 0usize);
            for inst in &code {
                let name = inst.name().unwrap_or("?").to_owned();
                if name.starts_with("ds_") {
                    lds += 1;
                }
                if name.starts_with("s_cbranch") || name.starts_with("s_branch") {
                    branches += 1;
                }
                if name.starts_with("image_")
                    || name.starts_with("buffer_")
                    || name.starts_with("tbuffer_")
                    || name.starts_with("s_load")
                    || name.starts_with("s_buffer_load")
                {
                    mem += 1;
                }
                *mix.entry(name).or_default() += 1;
            }
            let mut rows: Vec<_> = mix.into_iter().collect();
            rows.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            println!(
                "== {path} wave64={} instrs={} branches={branches} lds={lds} mem={mem}",
                p.wave64,
                rows.iter().map(|r| r.1).sum::<usize>()
            );
            for (name, n) in rows.iter().take(22) {
                println!("  {n:6} {name}");
            }
        }
    }
}
