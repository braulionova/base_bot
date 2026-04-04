use tracing::warn;

const BOT_TOKEN: &str = "7700486521:AAFuu2ygokFNesm1uB6_JM96KxQwcc4q-dk";
const CHAT_ID: &str = "483428397";

pub async fn send(msg: &str) {
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage",
        BOT_TOKEN
    );
    let client = reqwest::Client::new();
    let _ = client
        .post(&url)
        .form(&[
            ("chat_id", CHAT_ID),
            ("text", msg),
            ("parse_mode", "Markdown"),
        ])
        .send()
        .await
        .map_err(|e| warn!("Telegram error: {}", e));
}
