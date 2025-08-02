mod module_bindings;
use module_bindings::*;

use serde::Deserialize;
use spacetimedb_sdk::{DbContext, Error, Table};
use tokio::sync::mpsc;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Deserialize)]
struct Config {
    pub webhook_url: String,
    pub cluster_url: String,
    pub region: String,
    pub token: String,
}

const CONFIG_EMPTY: &str = r#"{
    "webhook_url": "",
    "cluster_url": "",
    "region": "",
    "token": ""
}"#;

struct ChatMessage {
    pub username: String,
    pub content: String,
    pub timestamp: i32,
}

#[tokio::main]
async fn main() {
    let config = load_config(Path::new("config.json")).expect("failed to load config.json");

    if config.cluster_url.is_empty() || config.region.is_empty() || config.token.is_empty() {
        eprintln!("please fill out the configuration file (config.json)!");
        return;
    }

    let start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("prehistoric times?")
        .as_secs() as i32;
    println!("started at {}", start);

    let (tx, rx) = mpsc::unbounded_channel::<ChatMessage>();

    let ctx = DbConnection::builder()
        .with_uri(&config.cluster_url)
        .with_module_name(&config.region)
        .with_token(Some(config.token))
        .on_disconnect(on_disconnected)
        .on_connect_error(on_connect_error)
        .on_connect(move |ctx, _, _| on_connected(ctx, &tx))
        .build()
        .expect("failed to connect");

    let _ = tokio::join!(
        launch_connection(&ctx),
        consume_messages(rx, start, &config.webhook_url),
        close_connection(&ctx),
    );
}

fn load_config(path: &Path) -> anyhow::Result<Config> {
    if !path.exists() {
        std::fs::write(path, CONFIG_EMPTY)?;
    }
    let content = std::fs::read_to_string(&path)?;
    let config: Config = serde_json::from_str(&content)?;
    Ok(config)
}

async fn launch_connection(ctx: &DbConnection) -> anyhow::Result<()> {
    ctx.run_async().await?;
    Ok(())
}

fn on_connected(ctx: &DbConnection, tx: &mpsc::UnboundedSender<ChatMessage>) {
    println!("connected!");
    let (tx_msg, tx_mod) = (tx.clone(), tx.clone());
    ctx.db.chat_message_state()
        .on_insert(move |ctx, row| on_message(ctx, row, &tx_msg));
    ctx.db.user_moderation_state()
        .on_insert(move |ctx, row| on_moderation(ctx, row, &tx_mod));

    ctx.subscription_builder().subscribe([
        "SELECT * FROM chat_message_state",
        "SELECT * FROM claim_state",
        "SELECT * FROM empire_state",
        "SELECT * FROM player_username_state",
        "SELECT * FROM user_moderation_state",
    ]);
}

fn on_message(ctx: &EventContext, row: &ChatMessageState, tx: &mpsc::UnboundedSender<ChatMessage>) {
    let row = row.clone();
    match row.channel_id {
        6 | 5 => {
            let postfix = ctx.db.empire_state()
                .entity_id()
                .find(&row.target_id)
                .map_or_else(|| None, |e| Some(e.name));

            if postfix.is_none() { return; }
            tx.send(ChatMessage {
                username: format!("{} [{}]", row.username, postfix.unwrap()),
                content: row.text.replace('"', "\\\""),
                timestamp: row.timestamp,
            }).unwrap();
        },
        4 => {
            let postfix = ctx.db.claim_state()
                .entity_id()
                .find(&row.target_id)
                .map_or_else(|| None, |c| Some(c.name));

            if postfix.is_none() { return; }
            tx.send(ChatMessage {
                username: format!("{} [{}]", row.username, postfix.unwrap()),
                content: row.text.replace('"', "\\\""),
                timestamp: row.timestamp,
            }).unwrap();
        },
        3 => {
            tx.send(ChatMessage {
                username: row.username,
                content: row.text.replace('"', "\\\""),
                timestamp: row.timestamp,
            }).unwrap();
        }
        _ => return
    };
}

fn on_moderation(ctx: &EventContext, row: &UserModerationState, tx: &mpsc::UnboundedSender<ChatMessage>) {
    let user = ctx.db.player_username_state()
        .entity_id()
        .find(&row.target_entity_id)
        .map_or_else(|| None, |p| Some(p.username));

    if user.is_none() { return; }
    let user = user.unwrap();

    let message = match row.user_moderation_policy {
        UserModerationPolicy::PermanentBlockLogin =>
            format!("User {} has been banned from logging in permanently!", user),
        UserModerationPolicy::TemporaryBlockLogin =>
            format!("User {} has been banned from logging in until <t:{}:f>!", user, row.expiration_time.to_micros_since_unix_epoch() / 1_000_000),
        UserModerationPolicy::BlockChat =>
            format!("User {} has been banned from chatting until <t:{}:f>!", user, row.expiration_time.to_micros_since_unix_epoch() / 1_000_000),
        UserModerationPolicy::BlockConstruct =>
            format!("User {} has been banned from building until <t:{}:f>!", user, row.expiration_time.to_micros_since_unix_epoch() / 1_000_000),
    };

    tx.send(ChatMessage {
        username: "<<MODERATION>>".to_string(),
        content: message,
        timestamp: (row.created_time.to_micros_since_unix_epoch() / 1_000_000) as i32,
    }).unwrap();
}

async fn consume_messages(mut rx: mpsc::UnboundedReceiver<ChatMessage>, start: i32, webhook_url: &str) {
    let client = reqwest::Client::new();

    while let Some(msg) = rx.recv().await {
        if msg.timestamp < start {
            continue;
        }

        println!("{}: {}", msg.username, msg.content);
        if webhook_url.is_empty() {
            continue;
        }

        let payload = format!(r#"{{"username": "{}", "content": "{}"}}"#, msg.username, msg.content);
        let response = client
            .post(webhook_url)
            .header("Content-Type", "application/json")
            .body(payload)
            .send()
            .await;

        if !response.is_ok_and(|r| r.status().is_success()) {
            eprintln!("failed to send message");
        }
    }
}

fn on_connect_error(_ctx: &ErrorContext, err: Error) {
    eprintln!("connection error: {:?}", err);
    std::process::exit(1);
}

fn on_disconnected(_ctx: &ErrorContext, err: Option<Error>) {
    if let Some(err) = err {
        eprintln!("disconnected: {}", err);
        std::process::exit(1);
    } else {
        println!("disconnected.");
        std::process::exit(0);
    }
}

async fn close_connection(ctx: &DbConnection) -> anyhow::Result<()> {
    tokio::signal::ctrl_c().await?;
    println!("received ctrl-c!");
    ctx.disconnect()?;
    Ok(())
}
