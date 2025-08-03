mod module_bindings;
use module_bindings::{*, UserModerationPolicy::*};

mod glue;
use glue::{Config, Configurable, with_channel};

use std::time::{SystemTime, UNIX_EPOCH};
use spacetimedb_sdk::{DbContext, Table, Timestamp};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};


enum Message {
    Disconnect,
    Chat {
        username: String,
        content: String,
    }
}

impl Message {
    pub fn chat(username: String, content: String) -> Self {
        Self::Chat{ username, content: content.replace("\"", "\\\"") }
    }

    pub fn claim(username: String, claim: String, content: String) -> Self {
        Self::chat(format!("{} [{}]", username, claim), content)
    }

    pub fn empire(username: String, empire: String, content: String) -> Self {
        Self::chat(format!("{} [{}]", username, empire), content)
    }

    pub fn moderation(username: String, policy: &str, expiry: &str) -> Self {
        Self::chat(
            "<<MODERATION>>".to_string(),
            format!("User {} has been banned from {} {}!", username, policy, expiry),
        )
    }
}

#[tokio::main]
async fn main() {
    let config = Config::from("config.json").expect("failed to load config.json");

    if config.is_empty() {
        eprintln!("please fill out the configuration file (config.json)!");
        return;
    }

    let (tx, rx) = unbounded_channel::<Message>();

    let ctx = DbConnection::builder()
        .configure(&config)
        .on_connect(|_, _, _| println!("connected!"))
        .on_disconnect(|_, _| println!("disconnected!"))
        .build()
        .expect("failed to connect");

    ctx.db.chat_message_state().on_insert(with_channel(tx.clone(), on_message));
    ctx.db.user_moderation_state().on_insert(with_channel(tx.clone(), on_moderation));

    let start_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("prehistoric times?")
        .as_secs();

    ctx.subscription_builder().subscribe([
        "SELECT * FROM claim_state",
        "SELECT * FROM empire_state",
        "SELECT * FROM player_username_state",
        &format!("SELECT * FROM chat_message_state WHERE timestamp > {}", start_ms),
        //&format!("SELECT * FROM user_moderation_state WHERE created_time > {}", start_ms * 1_000_000),
    ]);

    let mut producer = Box::pin(ctx.run_async());
    let consumer = tokio::spawn(consume(rx, config.webhook_url()));

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            ctx.disconnect().unwrap();
            producer.await.unwrap();
            tx.send(Message::Disconnect).unwrap();
            consumer.await.unwrap();
        },
        _ = &mut producer => {
            tx.send(Message::Disconnect).unwrap();
            consumer.await.unwrap();
        },
    }
}

fn on_message(ctx: &EventContext, row: &ChatMessageState, tx: &UnboundedSender<Message>) {
    let row = row.clone();

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
                tx.send(Message::empire(row.username, empire, row.text)).unwrap();
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
                tx.send(Message::claim(row.username, claim, row.text)).unwrap();
            } else {
                eprintln!("no claim found for id {}", row.target_id);
            }
        },
        REGION => tx.send(Message::chat(row.username, row.text)).unwrap(),
        _ => (),
    };
}

fn on_moderation(ctx: &EventContext, row: &UserModerationState, tx: &UnboundedSender<Message>) {
    let user = ctx.db.player_username_state()
        .entity_id()
        .find(&row.target_entity_id)
        .map(|p| p.username);

    if let Some(user) = user {
        let message = match row.user_moderation_policy {
            PermanentBlockLogin =>
                Message::moderation(user, "logging in", "permanently"),
            TemporaryBlockLogin =>
                Message::moderation(user, "logging in", &as_expiry(row.expiration_time)),
            BlockChat =>
                Message::moderation(user, "chatting", &as_expiry(row.expiration_time)),
            BlockConstruct =>
                Message::moderation(user, "building", &as_expiry(row.expiration_time)),
        };
        tx.send(message).unwrap();
    } else {
        eprintln!("no player found for id {}", row.target_entity_id);
    }
}

fn as_expiry(expiry: Timestamp) -> String {
    format!("until <t:{}:f>!", expiry.to_micros_since_unix_epoch() / 1_000_000)
}

async fn consume(mut rx: UnboundedReceiver<Message>, webhook_url: String) {
    let client = reqwest::Client::new();

    while let Some(msg) = rx.recv().await {
        if let Message::Disconnect = msg { break }

        if let Message::Chat {username, content} = msg {
            println!("{}: {}", username, content);
            if webhook_url.is_empty() {
                continue;
            }

            let payload = format!(r#"{{"username": "{}", "content": "{}"}}"#, username, content);
            let response = client
                .post(&webhook_url)
                .header("Content-Type", "application/json")
                .body(payload)
                .send()
                .await;

            if !response.is_ok_and(|r| r.status().is_success()) {
                eprintln!("failed to send message");
            }
        }
    }
}