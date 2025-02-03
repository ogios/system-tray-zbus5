use futures::StreamExt;
use system_tray::stream_client::Client;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let client = Client::new().await.unwrap();
    let mut stream = client.create_stream().await.unwrap();

    while let Some(e) = stream.next().await {
        println!("{e:?}");
    }
}
