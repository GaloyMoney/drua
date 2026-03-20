use clap::Parser;

#[derive(Parser)]
#[command(name = "galoy-agents", about = "Galoy Agents CLI")]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
    println!("{}", mcp_gateway::hello());
}
