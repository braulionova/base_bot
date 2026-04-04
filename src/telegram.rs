use tracing::warn;

pub async fn send(msg: &str) {
    let token = match std::env::var("TG_TOKEN") {
        Ok(t) => t,
        Err(_) => return,
    };
    let chat = match std::env::var("TG_CHAT") {
        Ok(c) => c,
        Err(_) => return,
    };
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
    let client = reqwest::Client::new();
    let _ = client.post(&url).form(&[("chat_id", chat.as_str()), ("text", msg), ("parse_mode", "Markdown")]).send().await.map_err(|e| warn!("Telegram: {}", e));
}
