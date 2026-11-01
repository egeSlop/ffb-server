//! Command-line front end, shared by the `ffb-server` binary and host apps.
//!
//!   run     serve ws://127.0.0.1:47820 and drive the wheel (default)
//!   test    12 s hardware check: push, centering, weight, vibration
//!   config  print the settings file path and values

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::wheel::Wheel;
use crate::{FfbServer, config, server};

/// Runs a CLI command (`run`, `test`, `config`). `name` identifies the host in status messages.
pub fn run(args: &[&str], name: &str) -> anyhow::Result<()> {
    match args.first().copied() {
        None | Some("run") => {
            let srv = FfbServer::start(name, config::load())?;
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            ctrlc::set_handler(move || s.store(true, Ordering::SeqCst))?;
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
            }
            srv.stop();
            Ok(())
        }
        Some("test") => {
            let cfg = config::load();
            let (path, mut w) = Wheel::open_device(&cfg.device, cfg.invert)?;
            w.set_gain(cfg.master_gain)?;
            println!("wheel: {} ({})", w.name(), path.display());
            for (name, f, ms) in server::test_steps() {
                println!("  {name}");
                w.apply(&f)?;
                thread::sleep(Duration::from_millis(ms));
            }
            w.idle()?;
            println!("done");
            Ok(())
        }
        Some("config") => {
            let c = config::load();
            println!("{}\n\n{}", config::path().display(), toml::to_string_pretty(&c)?);
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command {other}; use run | test | config");
            std::process::exit(2);
        }
    }
}
