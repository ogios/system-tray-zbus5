use system_tray::client::Client;

// #[tokio::main(flavor = "current_thread")]
#[tokio::main]
async fn main() {
    let client = Client::new().await.unwrap();

    let mut tray_rx = client.subscribe();

    let initial_items = client.items();

    println!("Initial items: {initial_items:?}");

    // do something with initial items...

    while let Ok(ev) = tray_rx.recv().await {
        println!("{ev:?}"); // do something with event...
    }
}
