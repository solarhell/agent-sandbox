mod backend;
mod cli;
mod landlock_exec;
mod proto;
mod runner;
mod service;
mod store;
mod workspace_locks;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if landlock_exec::maybe_run_helper()? {
        return Ok(());
    }
    cli::run().await
}
