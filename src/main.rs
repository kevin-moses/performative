#[tokio::main]
async fn main() -> anyhow::Result<()> {
    performative_tui::run_tui().await
}
