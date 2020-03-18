#![deny(clippy::all)]
#![deny(rust_2018_idioms)]

use crate::chain::chain;
use crate::db::DbPool;
use crate::event_aggregator::EventAggregator;
use crate::middleware::auth;
use crate::middleware::channel::{channel_load, get_channel_id};
use crate::middleware::cors::{cors, Cors};
use crate::routes::channel::channel_status;
use crate::routes::event_aggregate::list_channel_event_aggregates;
use crate::routes::validator_message::{extract_params, list_validator_messages};
use chrono::Utc;
use futures::future::{BoxFuture, FutureExt};
use hyper::{Body, Method, Request, Response, StatusCode};
use lazy_static::lazy_static;
use primitives::adapter::Adapter;
use primitives::sentry::ValidationErrorResponse;
use primitives::{Config, ValidatorId};
use redis::aio::MultiplexedConnection;
use regex::Regex;
use routes::analytics::{advanced_analytics, advertiser_analytics, analytics, publisher_analytics};
use routes::cfg::config;
use routes::channel::{
    channel_list, channel_validate, create_channel, create_validator_messages, insert_events,
    last_approved,
};
use slog::{error, Logger};
use std::collections::HashMap;

pub mod middleware {
    pub mod auth;
    pub mod channel;
    pub mod cors;
}

pub mod routes {
    pub mod analytics;
    pub mod cfg;
    pub mod channel;
    pub mod event_aggregate;
    pub mod validator_message;
}

pub mod access;
pub mod analytics_recorder;
mod chain;
pub mod db;
pub mod event_aggregator;
pub mod event_reducer;

lazy_static! {
    static ref CHANNEL_GET_BY_ID: Regex =
        Regex::new(r"^/channel/0x([a-zA-Z0-9]{64})/?$").expect("The regex should be valid");
    static ref LAST_APPROVED_BY_CHANNEL_ID: Regex = Regex::new(r"^/channel/0x([a-zA-Z0-9]{64})/last-approved/?$").expect("The regex should be valid");
    static ref CHANNEL_STATUS_BY_CHANNEL_ID: Regex = Regex::new(r"^/channel/0x([a-zA-Z0-9]{64})/status/?$").expect("The regex should be valid");
    // Only the initial Regex to be matched.
    static ref CHANNEL_VALIDATOR_MESSAGES: Regex = Regex::new(r"^/channel/0x([a-zA-Z0-9]{64})/validator-messages(/.*)?$").expect("The regex should be valid");
    static ref CHANNEL_EVENTS_AGGREGATES: Regex = Regex::new(r"^/channel/0x([a-zA-Z0-9]{64})/events-aggregates/?$").expect("The regex should be valid");
    static ref ANALYTICS_BY_CHANNEL_ID: Regex = Regex::new(r"^/analytics/0x([a-zA-Z0-9]{64})/?$").expect("The regex should be valid");
    static ref ADVERTISER_ANALYTICS_BY_CHANNEL_ID: Regex = Regex::new(r"^/analytics/for-advertiser/0x([a-zA-Z0-9]{64})/?$").expect("The regex should be valid");
    static ref PUBLISHER_ANALYTICS_BY_CHANNEL_ID: Regex = Regex::new(r"^/analytics/for-publisher/0x([a-zA-Z0-9]{64})/?$").expect("The regex should be valid");
    static ref CREATE_EVENTS_BY_CHANNEL_ID: Regex = Regex::new(r"^/channel/0x([a-zA-Z0-9]{64})/events(/.*)?$").expect("The regex should be valid");

}

fn auth_required_middleware<'a, A: Adapter>(
    req: Request<Body>,
    _: &Application<A>,
) -> BoxFuture<'a, Result<Request<Body>, ResponseError>> {
    async move {
        if req.extensions().get::<Session>().is_some() {
            Ok(req)
        } else {
            Err(ResponseError::Unauthorized)
        }
    }
    .boxed()
}

#[derive(Debug)]
pub struct RouteParams(Vec<String>);

impl RouteParams {
    pub fn get(&self, index: usize) -> Option<String> {
        self.0.get(index).map(ToOwned::to_owned)
    }

    pub fn index(&self, i: usize) -> String {
        self.0[i].clone()
    }
}

#[derive(Clone)]
pub struct Application<A: Adapter> {
    pub adapter: A,
    pub logger: Logger,
    pub redis: MultiplexedConnection,
    pub pool: DbPool,
    pub config: Config,
    pub event_aggregator: EventAggregator,
    __secret: (),
}

impl<A: Adapter + 'static> Application<A> {
    pub fn new(
        adapter: A,
        config: Config,
        logger: Logger,
        redis: MultiplexedConnection,
        pool: DbPool,
    ) -> Self {
        Self {
            adapter,
            config,
            logger,
            redis,
            pool,
            event_aggregator: Default::default(),
            __secret: (),
        }
    }

    pub async fn handle_routing(&self, req: Request<Body>) -> Response<Body> {
        let headers = match cors(&req) {
            Some(Cors::Simple(headers)) => headers,
            // if we have a Preflight, just return the response directly
            Some(Cors::Preflight(response)) => return response,
            None => Default::default(),
        };

        let req = match auth::for_request(req, &self.adapter, self.redis.clone()).await {
            Ok(req) => req,
            Err(error) => {
                error!(&self.logger, "{}", &error; "module" => "middleware-auth");
                return map_response_error(ResponseError::Unauthorized);
            }
        };

        let path = req.uri().path().to_string();
        let mut response = match (path.as_ref(), req.method()) {
            ("/cfg", &Method::GET) => config(req, &self).await,
            ("/channel", &Method::POST) => create_channel(req, &self).await,
            ("/channel/list", &Method::GET) => channel_list(req, &self).await,
            ("/channel/validate", &Method::POST) => channel_validate(req, &self).await,

            ("/analytics", &Method::GET) => analytics(req, &self).await,
            ("/analytics/advanced", &Method::GET) => {
                let req = match chain(req, &self, vec![Box::new(auth_required_middleware)]).await {
                    Ok(req) => req,
                    Err(error) => {
                        return map_response_error(error);
                    }
                };

                advanced_analytics(req, &self).await
            }
            ("/analytics/for-advertiser", &Method::GET) => {
                let req = match chain(req, &self, vec![Box::new(auth_required_middleware)]).await {
                    Ok(req) => req,
                    Err(error) => {
                        return map_response_error(error);
                    }
                };
                advertiser_analytics(req, &self).await
            }
            ("/analytics/for-publisher", &Method::GET) => {
                let req = match chain(req, &self, vec![Box::new(auth_required_middleware)]).await {
                    Ok(req) => req,
                    Err(error) => {
                        return map_response_error(error);
                    }
                };

                publisher_analytics(req, &self).await
            }
            (route, _) if route.starts_with("/analytics") => analytics_router(req, &self).await,
            // This is important becuase it prevents us from doing
            // expensive regex matching for routes without /channel
            (path, _) if path.starts_with("/channel") => channels_router(req, self).await,
            _ => Err(ResponseError::NotFound),
        }
        .unwrap_or_else(map_response_error);

        // extend the headers with the initial headers we have from CORS (if there are some)
        response.headers_mut().extend(headers);
        response
    }
}

async fn analytics_router<A: Adapter + 'static>(
    mut req: Request<Body>,
    app: &Application<A>,
) -> Result<Response<Body>, ResponseError> {
    let (route, method) = (req.uri().path(), req.method());

    match *method {
        Method::GET => {
            if let Some(caps) = ANALYTICS_BY_CHANNEL_ID.captures(route) {
                let param = RouteParams(vec![caps
                    .get(1)
                    .map_or("".to_string(), |m| m.as_str().to_string())]);
                req.extensions_mut().insert(param);

                let req = chain(
                    req,
                    app,
                    vec![Box::new(channel_load), Box::new(get_channel_id)],
                )
                .await?;

                analytics(req, app).await
            } else if let Some(caps) = ADVERTISER_ANALYTICS_BY_CHANNEL_ID.captures(route) {
                let param = RouteParams(vec![caps
                    .get(1)
                    .map_or("".to_string(), |m| m.as_str().to_string())]);
                req.extensions_mut().insert(param);

                let req = chain(
                    req,
                    app,
                    vec![Box::new(auth_required_middleware), Box::new(get_channel_id)],
                )
                .await?;

                advertiser_analytics(req, app).await
            } else if let Some(caps) = PUBLISHER_ANALYTICS_BY_CHANNEL_ID.captures(route) {
                let param = RouteParams(vec![caps
                    .get(1)
                    .map_or("".to_string(), |m| m.as_str().to_string())]);
                req.extensions_mut().insert(param);

                let req = chain(
                    req,
                    app,
                    vec![Box::new(auth_required_middleware), Box::new(get_channel_id)],
                )
                .await?;

                publisher_analytics(req, app).await
            } else {
                Err(ResponseError::NotFound)
            }
        }
        _ => Err(ResponseError::NotFound),
    }
}

async fn channels_router<A: Adapter + 'static>(
    mut req: Request<Body>,
    app: &Application<A>,
) -> Result<Response<Body>, ResponseError> {
    let (path, method) = (req.uri().path().to_owned(), req.method());

    // example with
    // @TODO remove later
    // regex matching for routes with params
    if let (Some(caps), &Method::GET) = (LAST_APPROVED_BY_CHANNEL_ID.captures(&path), method) {
        let param = RouteParams(vec![caps
            .get(1)
            .map_or("".to_string(), |m| m.as_str().to_string())]);
        req.extensions_mut().insert(param);

        last_approved(req, app).await
    } else if let (Some(caps), &Method::GET) =
        (CHANNEL_STATUS_BY_CHANNEL_ID.captures(&path), method)
    {
        let param = RouteParams(vec![caps
            .get(1)
            .map_or("".to_string(), |m| m.as_str().to_string())]);
        req.extensions_mut().insert(param);

        let req = channel_load(req, app).await?;
        channel_status(req, app).await
    } else if let (Some(caps), &Method::GET) = (CHANNEL_VALIDATOR_MESSAGES.captures(&path), method)
    {
        let param = RouteParams(vec![caps
            .get(1)
            .map_or("".to_string(), |m| m.as_str().to_string())]);

        req.extensions_mut().insert(param);

        let req = match chain(req, app, vec![Box::new(channel_load)]).await {
            Ok(req) => req,
            Err(error) => {
                return Err(error);
            }
        };

        // @TODO: Move this to a middleware?!
        let extract_params = match extract_params(caps.get(2).map_or("", |m| m.as_str())) {
            Ok(params) => params,
            Err(error) => {
                return Err(error.into());
            }
        };

        list_validator_messages(req, &app, &extract_params.0, &extract_params.1).await
    } else if let (Some(caps), &Method::POST) = (CHANNEL_VALIDATOR_MESSAGES.captures(&path), method)
    {
        let param = RouteParams(vec![caps
            .get(1)
            .map_or("".to_string(), |m| m.as_str().to_string())]);

        req.extensions_mut().insert(param);

        let req = match chain(
            req,
            app,
            vec![Box::new(auth_required_middleware), Box::new(channel_load)],
        )
        .await
        {
            Ok(req) => req,
            Err(error) => {
                return Err(error);
            }
        };

        create_validator_messages(req, &app).await
    } else if let (Some(caps), &Method::GET) = (CHANNEL_EVENTS_AGGREGATES.captures(&path), method) {
        if req.extensions().get::<Session>().is_none() {
            return Err(ResponseError::Unauthorized);
        }

        let param = RouteParams(vec![
            caps.get(1)
                .map_or("".to_string(), |m| m.as_str().to_string()),
            caps.get(2)
                .map_or("".to_string(), |m| m.as_str().trim_matches('/').to_string()),
        ]);
        req.extensions_mut().insert(param);

        let req = chain(req, app, vec![Box::new(channel_load)]).await?;

        list_channel_event_aggregates(req, app).await
    } else if let (Some(caps), &Method::POST) =
        (CREATE_EVENTS_BY_CHANNEL_ID.captures(&path), method)
    {
        let param = RouteParams(vec![caps
            .get(1)
            .map_or("".to_string(), |m| m.as_str().to_string())]);

        req.extensions_mut().insert(param);

        let req = match chain(req, app, vec![Box::new(channel_load)]).await {
            Ok(req) => req,
            Err(error) => {
                return Err(error);
            }
        };

        insert_events(req, app).await
    } else {
        Err(ResponseError::NotFound)
    }
}

#[derive(Debug)]
pub enum ResponseError {
    NotFound,
    BadRequest(String),
    FailedValidation(String),
    Unauthorized,
    Forbidden(String),
    Conflict(String),
    TooManyRequests(String),
}

impl<T> From<T> for ResponseError
where
    T: std::error::Error + 'static,
{
    fn from(error: T) -> Self {
        // @TODO use a error proper logger?
        println!("{:#?}", error);
        ResponseError::BadRequest("Bad Request: try again later".into())
    }
}

pub fn map_response_error(error: ResponseError) -> Response<Body> {
    match error {
        ResponseError::NotFound => not_found(),
        ResponseError::BadRequest(e) => bad_response(e, StatusCode::BAD_REQUEST),
        ResponseError::Unauthorized => bad_response(
            "invalid authorization".to_string(),
            StatusCode::UNAUTHORIZED,
        ),
        ResponseError::Forbidden(e) => bad_response(e, StatusCode::FORBIDDEN),
        ResponseError::Conflict(e) => bad_response(e, StatusCode::CONFLICT),
        ResponseError::TooManyRequests(e) => bad_response(e, StatusCode::TOO_MANY_REQUESTS),
        ResponseError::FailedValidation(e) => bad_validation_response(e),
    }
}

pub fn not_found() -> Response<Body> {
    let mut response = Response::new(Body::from("Not found"));
    let status = response.status_mut();
    *status = StatusCode::NOT_FOUND;
    response
}

pub fn bad_response(response_body: String, status_code: StatusCode) -> Response<Body> {
    let mut error_response = HashMap::new();
    error_response.insert("message", response_body);

    let body = Body::from(serde_json::to_string(&error_response).expect("serialise err response"));

    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert("Content-type", "application/json".parse().unwrap());

    *response.status_mut() = status_code;

    response
}

pub fn bad_validation_response(response_body: String) -> Response<Body> {
    let error_response = ValidationErrorResponse {
        status_code: 400,
        message: response_body.clone(),
        validation: vec![response_body],
    };

    let body = Body::from(serde_json::to_string(&error_response).expect("serialise err response"));

    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert("Content-type", "application/json".parse().unwrap());

    *response.status_mut() = StatusCode::BAD_REQUEST;

    response
}

pub fn success_response(response_body: String) -> Response<Body> {
    let body = Body::from(response_body);

    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert("Content-type", "application/json".parse().unwrap());

    let status = response.status_mut();
    *status = StatusCode::OK;

    response
}

pub fn epoch() -> f64 {
    Utc::now().timestamp() as f64 / 2_628_000_000.0
}

// @TODO: Make pub(crate)
#[derive(Debug, Clone)]
pub struct Session {
    pub era: i64,
    pub uid: ValidatorId,
    pub ip: Option<String>,
    pub country: Option<String>,
    pub referrer_header: Option<String>,
}
