//! Background-only information-panel microbenchmark. No UI or image cache.
#[allow(dead_code)]
#[path = "../src/exif_text.rs"]
mod exif_text;
#[allow(dead_code)]
#[path = "../src/image_info.rs"]
mod image_info;
#[allow(dead_code)]
#[path = "../src/png_metadata.rs"]
mod png_metadata;
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
    time::Instant,
};
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(args.len() == 3, "png_info IMAGE ROUNDS");
    let rounds: usize = args[2].parse()?;
    let mut rows = Vec::new();
    for _ in 0..rounds {
        let start = Instant::now();
        let fields = image_info::read(Path::new(&args[1]), None)?;
        let us = start.elapsed().as_micros();
        let mut hash = DefaultHasher::new();
        for field in &fields {
            field.name.hash(&mut hash);
            field.value.hash(&mut hash);
        }
        rows.push(serde_json::json!({"us":us,"fields":fields.len(),"hash":hash.finish()}));
    }
    println!("{}", serde_json::json!({"samples":rows}));
    Ok(())
}
