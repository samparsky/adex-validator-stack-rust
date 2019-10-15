use crate::error::ValidatorWorker;
use chrono::{DateTime, Utc};
use futures::compat::Future01CompatExt;
use futures::future::try_join_all;
use futures::lock::Mutex;
use futures_legacy::Future as LegacyFuture;
use primitives::adapter::Adapter;
use primitives::sentry::{
    ChannelAllResponse, EventAggregateResponse, LastApprovedResponse, SuccessResponse,
    ValidatorMessageResponse,
};
use primitives::validator::MessageTypes;
use primitives::{Channel, Config, ValidatorDesc};
use reqwest::header::AUTHORIZATION;
use reqwest::r#async::{Client, Response};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct SentryApi<T: Adapter> {
    pub adapter: Arc<Mutex<T>>,
    pub validator_url: String,
    pub client: Client,
    pub logging: bool,
    pub channel: Channel,
    pub config: Config,
    pub whoami: String,
}

impl<T: Adapter + 'static> SentryApi<T> {
    pub fn init(
        adapter: Arc<Mutex<T>>,
        channel: &Channel,
        config: &Config,
        logging: bool,
        whoami: &str,
    ) -> Result<Self, ValidatorWorker> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.fetch_timeout.into()))
            .build()
            .unwrap();

        // validate that we are to validate the channel
        match channel
            .spec
            .validators
            .into_iter()
            .find(|&v| v.id == whoami)
        {
            Some(v) => {
                let validator_url = format!("{}/channel/{}", v.url, channel.id);

                Ok(Self {
                    adapter,
                    validator_url,
                    client,
                    logging,
                    channel: channel.to_owned(),
                    config: config.to_owned(),
                    whoami: whoami.to_owned(),
                })
            }
            None => Err(ValidatorWorker::Failed(
                "we can not find validator entry for whoami".to_string(),
            )),
        }
    }

    pub async fn propagate(&self, messages: &[&MessageTypes]) {
        let serialised_messages: Vec<String> = messages
            .iter()
            .map(|message| serde_json::to_string(message).expect("failed to serialise message"))
            .collect();

        let mut adapter = self.adapter.lock().await;

        for validator in self.channel.spec.validators.into_iter() {
            let auth_token = adapter.get_auth(&validator.id);

            if let Err(e) = auth_token {
                println!("propagate error: get auth failed {}", e);
                continue;
            }

            if let Err(e) = propagate_to(
                &auth_token.unwrap(),
                self.config.propagation_timeout,
                &validator,
                &serialised_messages,
            ) {
                handle_http_error(e, &validator.url)
            }
        }
    }

    pub async fn get_latest_msg(
        &self,
        from: String,
        message_type: String,
    ) -> Result<Option<MessageTypes>, reqwest::Error> {
        let future = self
            .client
            .get(&format!(
                "{}/validator-messages/{}/{}?limit=1",
                self.validator_url, from, message_type
            ))
            .send()
            .and_then(|mut res: Response| res.json::<ValidatorMessageResponse>())
            .compat();

        let response = future.await?;
        match response {
            ValidatorMessageResponse::ValidatorMessages(Some(data)) => {
                if !data.is_empty() {
                    return Ok(Some(data[0].msg.clone()));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    pub async fn get_our_latest_msg(
        &self,
        message_type: String,
    ) -> Result<Option<MessageTypes>, reqwest::Error> {
        self.get_latest_msg(self.whoami.clone(), message_type).await
    }

    pub async fn get_last_approved(&self) -> Result<LastApprovedResponse, reqwest::Error> {
        let future = self
            .client
            .get(&format!("{}/last-approved", self.validator_url))
            .send()
            .and_then(|mut res: Response| res.json::<LastApprovedResponse>());

        future.compat().await
    }

    pub async fn get_last_msgs(&self) -> Result<LastApprovedResponse, reqwest::Error> {
        let future = self
            .client
            .get(&format!(
                "{}/last-approved?withHearbeat=true",
                self.validator_url
            ))
            .send()
            .and_then(|mut res: Response| res.json::<LastApprovedResponse>());

        future.compat().await
    }

    pub async fn get_event_aggregates(
        &self,
        after: DateTime<Utc>,
    ) -> Result<EventAggregateResponse, Box<ValidatorWorker>> {
        let mut adapter = self.adapter.lock().await;

        let auth_token = adapter
            .get_auth(&self.whoami)
            .map_err(|e| Box::new(ValidatorWorker::Failed(e.to_string())))?;

        let url = format!(
            "{}/events-aggregates?after={}",
            self.validator_url,
            after.timestamp()
        );

        let future = self
            .client
            .get(&url)
            .header(AUTHORIZATION, auth_token.to_string())
            .send()
            .and_then(|mut res: Response| res.json::<EventAggregateResponse>())
            .map_err(|e| Box::new(ValidatorWorker::Failed(e.to_string())));

        future.compat().await
    }
}

fn propagate_to(
    auth_token: &str,
    timeout: u32,
    validator: &ValidatorDesc,
    messages: &[String],
) -> Result<(), reqwest::Error> {
    // create client with timeout
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout.into()))
        .build()?;
    let url = validator.url.to_string();

    let _response: SuccessResponse = client
        .post(&url)
        .header(AUTHORIZATION, auth_token.to_string())
        .json(messages)
        .send()?
        .json()?;

    Ok(())
}

fn handle_http_error(e: reqwest::Error, url: &str) {
    if e.is_http() {
        match e.url() {
            None => println!("No Url given"),
            Some(url) => println!("Problem making request to: {}", url),
        }
    }
    // Inspect the internal error and output it
    if e.is_serialization() {
        let serde_error = match e.get_ref() {
            None => return,
            Some(err) => err,
        };
        println!("problem parsing information {}", serde_error);
    }

    if e.is_client_error() {
        println!("erorr sending http request for validator {}", url)
    }

    if e.is_redirect() {
        println!("server redirecting too many times or making loop");
    }
}

pub async fn all_channels(
    sentry_url: &str,
    whoami: String,
) -> Result<Vec<Channel>, reqwest::Error> {
    let url = sentry_url.to_owned();
    let first_page = fetch_page(url.clone(), 0, whoami.clone()).await?;

    if first_page.total_pages < 2 {
        Ok(first_page.channels)
    } else {
        let mut all: Vec<ChannelAllResponse> = try_join_all(
            (0..first_page.total_pages).map(|i| fetch_page(url.clone(), i, whoami.clone())),
        )
        .await?;

        all.push(first_page);
        let result_all: Vec<Channel> = all
            .into_iter()
            .flat_map(|ch| ch.channels.into_iter())
            .collect();
        Ok(result_all)
    }
}

async fn fetch_page(
    sentry_url: String,
    page: u64,
    validator: String,
) -> Result<ChannelAllResponse, reqwest::Error> {
    let client = Client::new();

    let mut query = vec![format!("page={}", page)];
    query.push(format!("validator={}", validator.to_string()));

    let future = client
        .get(format!("{}/channel/list?{}", sentry_url, query.join("&")).as_str())
        .send()
        .and_then(|mut res: Response| res.json::<ChannelAllResponse>());

    future.compat().await
}
