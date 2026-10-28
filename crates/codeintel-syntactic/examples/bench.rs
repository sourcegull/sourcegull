//! Times `analyze` over files: `cargo run --release -p sourcegull-codeintel-syntactic --example bench -- <lang> <files...>`.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let files: Vec<Vec<u8>> = args[2..]
        .iter()
        .map(|f| std::fs::read(f).expect("read"))
        .collect();
    let bytes: usize = files.iter().map(Vec::len).sum();
    let t = std::time::Instant::now();
    let mut occ = 0;
    for f in &files {
        occ +=
            sourcegull_codeintel_syntactic::analyze(&args[1], f).map_or(0, |l| l.occurrences.len());
    }
    let el = t.elapsed();
    println!(
        "{} files, {} KB, {occ} occurrences in {el:?} ({:.1} MB/s)",
        files.len(),
        bytes / 1024,
        bytes as f64 / el.as_secs_f64() / 1e6
    );
}
