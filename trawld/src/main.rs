use std::{process, error::Error, future::pending};
use clap::Parser;
use trawld::{log::Logger, parser::CliArgs, snapshot, ResourceManager};


#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = match CliArgs::try_parse() {
        Ok(args) => args,
        // --help and --version aren't errors: print them and exit 0
        Err(e) if !e.use_stderr() => e.exit(),
        Err(e) => {
            eprintln!("Error parsing: {e}");
            process::exit(1);
        }
    };
    let manager = ResourceManager::from_args(&args);
    let _conn = match snapshot::runtime_dir() {
        Ok(dir) => manager.run_server_with_snapshot(dir).await?,
        Err(e) => {
            Logger::from(&args).error(&e);
            manager.run_server().await?
        }
    };
    pending::<()>().await;
    Ok(())
}
