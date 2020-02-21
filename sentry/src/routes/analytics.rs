use crate::db::analytics::{
    advertiser_channel_ids, get_advanced_reports, get_analytics, AnalyticsType,
};
use crate::success_response;
use crate::Application;
use crate::ResponseError;
use crate::RouteParams;
use crate::Session;
use hyper::{Body, Request, Response};
use primitives::adapter::Adapter;
use primitives::analytics::{AnalyticsQuery, AnalyticsResponse};
use primitives::ChannelId;
use redis::aio::MultiplexedConnection;
use slog::{error, Logger};

pub async fn publisher_analytics<A: Adapter>(
    req: Request<Body>,
    app: &Application<A>,
) -> Result<Response<Body>, ResponseError> {
    let sess = req.extensions().get::<Session>();

    let analytics_type = AnalyticsType::Publisher {
        session: sess.cloned().ok_or(ResponseError::Unauthorized)?,
    };

    process_analytics(req, app, analytics_type)
        .await
        .map(success_response)
}

pub async fn analytics<A: Adapter>(
    req: Request<Body>,
    app: &Application<A>,
) -> Result<Response<Body>, ResponseError> {
    let request_uri = req.uri().to_string();
    let redis = app.redis.clone();

    match redis::cmd("GET")
        .arg(&request_uri)
        .query_async::<_, Option<String>>(&mut redis.clone())
        .await
    {
        Ok(Some(response)) => Ok(success_response(response)),
        _ => {
            // checks if /:id route param is present
            let cache_timeframe = match req.extensions().get::<RouteParams>() {
                Some(_) => 600,
                None => 300,
            };
            let response = process_analytics(req, app, AnalyticsType::Global).await?;
            cache(
                &redis.clone(),
                request_uri,
                &response,
                cache_timeframe,
                &app.logger,
            )
            .await;
            Ok(success_response(response))
        }
    }
}

pub async fn advertiser_analytics<A: Adapter>(
    req: Request<Body>,
    app: &Application<A>,
) -> Result<Response<Body>, ResponseError> {
    let sess = req.extensions().get::<Session>();
    let analytics_type = AnalyticsType::Advertiser {
        session: sess.ok_or(ResponseError::Unauthorized)?.to_owned(),
    };

    process_analytics(req, app, analytics_type)
        .await
        .map(success_response)
}

pub async fn process_analytics<A: Adapter>(
    req: Request<Body>,
    app: &Application<A>,
    analytics_type: AnalyticsType,
) -> Result<String, ResponseError> {
    let mut query = serde_urlencoded::from_str::<AnalyticsQuery>(&req.uri().query().unwrap_or(""))?;
    query
        .is_valid()
        .map_err(|e| ResponseError::BadRequest(e.to_string()))?;

    let channel_id = req.extensions().get::<ChannelId>();

    let segment_channel = query
        .segment_by_channel
        .clone()
        .map(|_| true)
        .unwrap_or_else(|| false);
    let limit = query.limit;

    let aggr = get_analytics(
        query,
        &app.pool,
        analytics_type,
        segment_channel,
        channel_id,
    )
    .await?;

    let response = AnalyticsResponse { limit, aggr };

    serde_json::to_string(&response)
        .map_err(|_| ResponseError::BadRequest("error occurred; try again later".to_string()))
}

pub async fn advanced_analytics<A: Adapter>(
    req: Request<Body>,
    app: &Application<A>,
) -> Result<Response<Body>, ResponseError> {
    let sess = req.extensions().get::<Session>().expect("auth is required");
    let advertiser_channels = advertiser_channel_ids(&app.pool, &sess.uid).await?;

    let query = serde_urlencoded::from_str::<AnalyticsQuery>(&req.uri().query().unwrap_or(""))?;

    let response = get_advanced_reports(
        &app.redis,
        &query.event_type,
        &sess.uid,
        &advertiser_channels,
    )
    .await
    .map_err(|_| ResponseError::BadRequest("error occurred; try again later".to_string()))?;

    Ok(success_response(serde_json::to_string(&response)?))
}

async fn cache(
    redis: &MultiplexedConnection,
    key: String,
    value: &str,
    timeframe: i32,
    logger: &Logger,
) {
    if let Err(err) = redis::cmd("SETEX")
        .arg(&key)
        .arg(timeframe)
        .arg(value)
        .query_async::<_, ()>(&mut redis.clone())
        .await
    {
        error!(&logger, "Server error: {}", err; "module" => "analytics-cache");
    }
}
