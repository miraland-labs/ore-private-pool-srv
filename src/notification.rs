// use serenity::builder::ExecuteWebhook;
use {
    serenity::{http::Http, model::webhook::Webhook},
    slack_messaging::Message as SlackChannelMessage,
    std::{fmt, io, str::FromStr, time::Duration},
    tokio::sync::mpsc::UnboundedReceiver,
    tracing::{error, info, warn},
};

#[derive(Debug)]
pub enum RewardsMessage {
    // Rewards(/* difficulty: */ u32, /* rewards: */ f64, /* balance: */ f64),
    Rewards(u32, f64, f64),
}

#[derive(Debug)]
enum SrcType {
    Pool,
    Solo,
}

impl fmt::Display for SrcType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SrcType::Pool => write!(f, "pool"),
            SrcType::Solo => write!(f, "solo"),
        }
    }
}

impl FromStr for SrcType {
    type Err = io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pool" => Ok(SrcType::Pool),
            "solo" => Ok(SrcType::Solo),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "Unknown source type")),
        }
    }
}

pub(crate) async fn slack_messaging_system(
    slack_webhook: String,
    mut receiver_channel: UnboundedReceiver<RewardsMessage>,
) {
    loop {
        while let Some(slack_message) = receiver_channel.recv().await {
            match slack_message {
                RewardsMessage::Rewards(d, r, b) =>
                    slack_messaging(slack_webhook.clone(), SrcType::Pool, d, r, b).await,
            }
        }
    }
}

pub(crate) async fn discord_messaging_system(
    discord_webhook: String,
    mut receiver_channel: UnboundedReceiver<RewardsMessage>,
) {
    loop {
        while let Some(discord_message) = receiver_channel.recv().await {
            match discord_message {
                RewardsMessage::Rewards(d, r, b) =>
                    discord_messaging(discord_webhook.clone(), SrcType::Pool, d, r, b).await,
            }
        }
    }
}

async fn slack_messaging(
    slack_webhook: String,
    source: SrcType,
    difficulty: u32,
    rewards: f64,
    balance: f64,
) {
    let text = format!("S: {}\nD: {}\nR: {}\nB: {}", source, difficulty, rewards, balance);
    let slack_webhook_url =
        url::Url::parse(&slack_webhook).expect("Failed to parse slack webhook url");
    let message = SlackChannelMessage::builder().text(text).build();
    let req = reqwest::Client::new().post(slack_webhook_url).json(&message);
    let mut num_retries = 0;
    loop {
        if let Err(err) = req.try_clone().unwrap().send().await {
            // eprintln!("{}", err);
            // error!("{}", err);
            error!("Err sending slack webhook: {:?}", err);
            if num_retries < 3 {
                info!("retry...");
                num_retries += 1;
                tokio::time::sleep(Duration::from_millis(1_000)).await;
                continue;
            } else {
                warn!("Failed 3 attempts to send message to slack. No more retry.");
            }
        }
        break;
    }
}

async fn discord_messaging(
    discord_webhook: String,
    source: SrcType,
    difficulty: u32,
    rewards: f64,
    balance: f64,
) {
    let text = format!("S: {}\nD: {}\nR: {}\nB: {}", source, difficulty, rewards, balance);
    // You don't need a token when you are only dealing with webhooks.
    let http = Http::new("");
    let discord_webhook = Webhook::from_url(&http, &discord_webhook)
        .await
        .expect("Failed to parse discord webhook url");

    // let builder = ExecuteWebhook::new().content(&text).username("Mirabot");
    // discord_webhook.execute(&http, false, builder).await.expect("Could not execute webhook.");

    let mut num_retries = 0;
    loop {
        // if let Err(err) = discord_webhook.execute(&http, false, builder).await {
        if let Err(err) =
            discord_webhook.execute(&http, false, |w| w.content(&text).username("Mirabot")).await
        {
            // eprintln!("{}", err);
            error!("Err sending discord webhook: {:?}", err);
            if num_retries < 3 {
                info!("retry...");
                num_retries += 1;
                tokio::time::sleep(Duration::from_millis(1_000)).await;
                continue;
            } else {
                warn!("Failed 3 attempts to send message to discord. No more retry.");
            }
        }
        break;
    }
}
