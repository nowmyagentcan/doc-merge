fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => println!("doc-merge {}", env!("CARGO_PKG_VERSION")),
        _ => {
            eprintln!("Usage: doc-merge --version");
            std::process::exit(2);
        }
    }
}
