mod module_bindings;
use module_bindings::*;
use UserModerationPolicy::*;

use serde::Deserialize;
use spacetimedb_sdk::{DbContext, Error, Table, Timestamp};
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

impl ChatMessage {
    pub fn new(username: String, content: String, timestamp: i32) -> Self {
        ChatMessage { username, content: content.replace("\"", "\\\""), timestamp }
    }

    pub fn claim(username: String, claim: String, content: String, timestamp: i32) -> Self {
        ChatMessage::new(format!("{} [{}]", username, claim), content, timestamp)
    }

    pub fn empire(username: String, empire: String, content: String, timestamp: i32) -> Self {
        ChatMessage::new(format!("{} [{}]", username, empire), content, timestamp)
    }

    pub fn moderation(username: String, policy: &str, expiry: &str, timestamp: i32) -> Self {
        ChatMessage::new(
            "<<MODERATION>>".to_string(),
            format!("User {} has been banned from {} {}!", username, policy, expiry),
            timestamp,
        )
    }
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
        .on_insert(move |ctx, row| on_message(ctx, row.clone(), &tx_msg));
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

fn on_message(ctx: &EventContext, row: ChatMessageState, tx: &mpsc::UnboundedSender<ChatMessage>) {
    const EMPIRE_INTERNAL: i32 = ChatChannel::EmpireInternal as i32;
    const EMPIRE_PUBLIC: i32 = ChatChannel::EmpirePublic as i32;
    const CLAIM: i32 = ChatChannel::Claim as i32;
    const REGION: i32 = ChatChannel::Region as i32;

    match row.channel_id {
        EMPIRE_INTERNAL | EMPIRE_PUBLIC => {
            let empire = ctx.db.empire_state()
                .entity_id()
                .find(&row.target_id)
                .map(|e| e.name);

            if let Some(empire) = empire {
                tx.send(ChatMessage::empire(row.username, empire, row.text, row.timestamp)).unwrap();
            } else {
                eprintln!("no empire found for id {}", row.target_id);
            }
        },
        CLAIM => {
            let claim = ctx.db.claim_state()
                .entity_id()
                .find(&row.target_id)
                .map(|c| c.name);

            if let Some(claim) = claim {
                tx.send(ChatMessage::claim(row.username, claim, row.text, row.timestamp)).unwrap();
            } else {
                eprintln!("no claim found for id {}", row.target_id);
            }
        },
        REGION => tx.send(ChatMessage::new(row.username, row.text, row.timestamp)).unwrap(),
        _ => (),
    };
}

fn on_moderation(ctx: &EventContext, row: &UserModerationState, tx: &mpsc::UnboundedSender<ChatMessage>) {
    let user = ctx.db.player_username_state()
        .entity_id()
        .find(&row.target_entity_id)
        .map(|p| p.username);

    if let Some(user) = user {
        let timestamp = (row.created_time.to_micros_since_unix_epoch() / 1_000_000) as i32;
        let message = match row.user_moderation_policy {
            PermanentBlockLogin =>
                ChatMessage::moderation(user, "logging in", "permanently", timestamp),
            TemporaryBlockLogin =>
                ChatMessage::moderation(user, "logging in", &as_expiry(row.expiration_time), timestamp),
            BlockChat =>
                ChatMessage::moderation(user, "chatting", &as_expiry(row.expiration_time), timestamp),
            BlockConstruct =>
                ChatMessage::moderation(user, "building", &as_expiry(row.expiration_time), timestamp),
        };
        tx.send(message).unwrap();
    } else {
        eprintln!("no player found for id {}", row.target_entity_id);
    }
}

fn as_expiry(expiry: Timestamp) -> String {
    format!("until <t:{}:f>!", expiry.to_micros_since_unix_epoch() / 1_000_000)
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
