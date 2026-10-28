//! Prints the syntax tree of a snippet: `cargo run -p sourcegull-codeintel-syntactic --example sexp -- <lang> <file>`.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let src = std::fs::read(&args[2]).expect("read");
    let lang: tree_sitter::Language = match args[1].as_str() {
        "python" => tree_sitter_python::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "cpp" => tree_sitter_cpp::LANGUAGE.into(),
        other => panic!("unknown {other}"),
    };
    let mut p = tree_sitter::Parser::new();
    p.set_language(&lang).expect("language");
    let t = p.parse(&src, None).expect("parse");
    println!("{}", t.root_node().to_sexp());
}
