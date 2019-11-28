use crate::Application;
use crate::ResponseError;
use hyper::{Body, Request, Response};
use primitives::adapter::Adapter;
use crate::RouteParams;
use chrono::{Utc};
use crate::Session;
use serde::{Serialize, Deserialize};
use bb8_postgres::tokio_postgres::Row;
use crate::success_response;
use std::cmp;
use redis::aio::MultiplexedConnection;


#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AnalyticsResponse {
    time: u32,
    value: String
}

impl From<&Row> for AnalyticsResponse {
    fn from(row: &Row) -> Self {
        Self {
            time: row.get("time"),
            value: row.get("value")
        }
    }
}

#[derive(Debug, Deserialize)]
struct AnalyticsQuery {
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default = "default_event_type")]
    pub event_type: String,
    #[serde(default = "default_metric")]
    pub metric: String,
    #[serde(default = "default_timeframe")]
    pub timeframe: String,   
}

fn default_limit() -> u32 {
    100
}

fn default_event_type() -> String {
    "IMPRESSION".into()
}

fn default_metric() -> String {
    "eventCounts".into()
}

fn default_timeframe() -> String {
    "hour".into()
}

pub async fn publisher_analytics<A: Adapter>(
    req: Request<Body>,
    app: &Application<A>,
) ->  Result<Response<Body>, ResponseError>  {
    process_analytics(req, app, false, false).await.map(success_response)
}

pub async fn analytics<A: Adapter>(
    req: Request<Body>,
    app: &Application<A>,
) -> Result<Response<Body>, ResponseError>   {
    let request_uri = req.uri().to_string();
    let redis = app.redis.clone();

    match redis::cmd("EXISTS").arg(&request_uri)
        .query_async::<_, String>(&mut redis.clone())
        .await 
    {
        Ok(response) => Ok(success_response(response)),
        _ => {
            let cache_timeframe = match req.extensions().get::<RouteParams>() {
                Some(_) => 600,
                None => 300
            };
            let response = process_analytics(req, app, false, true).await?;
            cache(&redis.clone(), request_uri, &response, cache_timeframe).await;        
            Ok(success_response(response))
        }
    }

}

pub async fn advertiser_analytics<A: Adapter>(
    req: Request<Body>,
    app: &Application<A>
) ->  Result<Response<Body>, ResponseError> {
    process_analytics(req, app, true, true).await.map(success_response)
}

pub async fn process_analytics<A: Adapter>(req: Request<Body>,  app: &Application<A>, is_advertiser: bool, skip_publisher: bool) -> Result<String, ResponseError>  {
    let query = serde_urlencoded::from_str::<AnalyticsQuery>(&req.uri().query().unwrap_or(""))?;
    let applied_limit = cmp::min(query.limit, 200);
    let (interval, period) = get_time_frame(&query.timeframe);
    let time_limit = Utc::now().timestamp() - period;
    let sess = req.extensions().get::<Session>();

    let mut where_clauses = vec![format!("created > to_timestamp({})", time_limit)];

    if is_advertiser {
        match req.extensions().get::<RouteParams>() {
            Some(params) => where_clauses.push(format!("channel_id IN ({})", params.index(0))),
            None => where_clauses.push(format!("channel_id IN (SELECT id FROM channels WHERE creator = {})", sess.unwrap().uid.to_string()))
        };      
    } else if let Some(params) = req.extensions().get::<RouteParams>() {
        if let Some(id) = params.get(0) {
            where_clauses.push(format!("channel_id = {}", id));
        };
    }

    let select_query = match (skip_publisher, sess) {
        (false, Some(session)) => {
            where_clauses.push(format!("events->'{}'->'{}'->'{}' IS NOT NULL", query.event_type, query.metric, session.uid));
            format!("select SUM((events->'{}'->'{}'->>'{}')::numeric) as value, extract({} from created) as time from event_aggregates", query.event_type, query.metric, session.uid, interval)
        }
        _ => {
            where_clauses.push(format!("events->'{}'->'{}' IS NOT NULL", query.event_type, query.metric));
            format!("select SUM(value::numeric)::varchar as value, extract({} from created) as time from event_aggregates, jsonb_each_text(events->'{}'->'{}')", interval, query.event_type, query.metric)
        }
    };

    let sql_query = format!("{} WHERE {} GROUP BY time LIMIT {}", select_query, where_clauses.join(" AND "), applied_limit);

    // log the query here
    println!("{}", sql_query);

    // execute query
    let result = app.pool
        .run(move |connection| {
            async move {
                match connection.prepare(&sql_query).await {
                    Ok(stmt) => match connection.query(&stmt, &[]).await {
                        Ok(rows) => {
                            let analytics: Vec<AnalyticsResponse> = rows.iter().map(AnalyticsResponse::from).collect();
                            Ok((analytics, connection))
                        },
                        Err(e) => Err((e, connection)),
                    },
                    Err(e) => Err((e, connection)),
                }
            }
        }).await?;
    
    serde_json::to_string(&result).map_err(|_| ResponseError::BadRequest("error occurred; try again later".to_string()))
}

async fn cache(redis: &MultiplexedConnection, key: String, value: &str, timeframe: i32) {
    if let Err(err) = redis::cmd("SETEX")
        .arg(&key)
        .arg(timeframe)
        .arg(value)
        .query_async::<_, ()>(&mut redis.clone())
        .await 
    {
        println!("{:?}", err);
    }
}

fn get_time_frame(timeframe: &str) -> (String, i64) {
    let minute = 60 * 1000;
    let hour = 60 * minute;
    let day = 24 * hour;
    
    match timeframe {
        "year"  =>  ("month".into(), 365 * day),
        "month" =>  ("day".into(), 30 * day),
        "week"  =>  ("week".into(), 7 * day),
        "day"   =>  ("hour".into(), day),
        "hour"  =>  ("minute".into(), hour),
        _       =>  ("hour".into(), day),
    }
}