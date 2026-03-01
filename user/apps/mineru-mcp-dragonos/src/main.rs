#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    mineru_mcp_dragonos::run().await
}
