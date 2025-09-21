use clap::Parser;

#[derive(Parser)]
#[command(name = "seekfs-bench", version)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _ = Cli::parse();
    println!("seekfs-bench: scaffolding ready");
}

