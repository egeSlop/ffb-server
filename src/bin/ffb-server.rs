//! Headless FFB server, for game launch scripts and machines without a GUI.
//! Usage: ffb-server [run | test | config]

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ffb_server::cli::run(&args.iter().map(String::as_str).collect::<Vec<_>>(), "ffb-server")
}
