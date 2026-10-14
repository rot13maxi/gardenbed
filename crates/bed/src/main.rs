//! bed — the tool you tend your gardenbed with.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some(cmd) => println!("bed {cmd}: not yet planted (stub)"),
        None => println!("bed — tend your gardenbed. Try: bed sow | graft | harvest | prune"),
    }
}
