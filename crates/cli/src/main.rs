//! Entry mỏng của `lh` — parse args rồi gọi lib. Toàn bộ logic ở `lib.rs`.

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    std::process::exit(cli::run(cli));
}
