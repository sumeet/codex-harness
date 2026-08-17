use codex_app_server_client::Client;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    smol::block_on(async {
        let client = Client::launch("codex")?;
        client
            .initialize("harness_probe", "Harness Probe", env!("CARGO_PKG_VERSION"))
            .await?;
        let result = client.list_threads(1, None).await?;
        let count = result.data.len();
        let has_more = result.next_cursor.is_some();
        let latest_turn_count = if let Some(thread) = result.data.first() {
            client.read_thread(&thread.id).await?.turns.len()
        } else {
            0
        };

        println!("Codex app-server reachable");
        println!("threads returned: {count}");
        println!(
            "more threads available: {}",
            if has_more { "yes" } else { "no" }
        );
        println!("turns in latest thread: {latest_turn_count}");
        Ok(())
    })
}
