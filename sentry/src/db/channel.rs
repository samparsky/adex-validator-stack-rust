use crate::db::DbPool;
use bb8::RunError;
use primitives::{Channel, ChannelId, ValidatorId};
use std::str::FromStr;

pub async fn get_channel_by_id(
    pool: &DbPool,
    id: &ChannelId,
) -> Result<Option<Channel>, RunError<bb8_postgres::tokio_postgres::Error>> {
    pool
        .run(move |connection| {
            async move {
                match connection.prepare("SELECT id, creator, deposit_asset, deposit_amount, valid_until, spec FROM channels WHERE id = $1 LIMIT 1").await {
                    Ok(select) => match connection.query(&select, &[&id]).await {
                        Ok(results) => Ok((results.get(0).map(Channel::from), connection)),
                        Err(e) => Err((e, connection)),
                    },
                    Err(e) => Err((e, connection)),
                }
            }
        })
        .await
}

pub async fn get_channel_by_id_and_validator(
    pool: &DbPool,
    id: &ChannelId,
    validator_id: &ValidatorId,
) -> Result<Option<Channel>, RunError<bb8_postgres::tokio_postgres::Error>> {
    pool
        .run(move |connection| {
            async move {
                let validator = serde_json::Value::from_str(&format!(r#"[{{"id": "{}"}}]"#, validator_id)).expect("Not a valid json");
                let query = "SELECT id, creator, deposit_asset, deposit_amount, valid_until, spec FROM channels WHERE id = $1 AND spec->'validators' @> $2 LIMIT 1";
                match connection.prepare(query).await {
                    Ok(select) => {
                        match connection.query(&select, &[&id, &validator]).await {
                            Ok(results) => Ok((results.get(0).map(Channel::from), connection)),
                            Err(e) => Err((e, connection)),
                        }
                    },
                    Err(e) => Err((e, connection)),
                }
            }
        })
        .await
}

pub async fn insert_channel(
    pool: &DbPool,
    channel: &Channel,
) -> Result<bool, RunError<bb8_postgres::tokio_postgres::Error>> {
    pool
        .run(move |connection| {
            async move {
                match connection.prepare("INSERT INTO channels (id, creator, deposit_asset, deposit_amount, valid_until, spec) values ($1, $2, $3, $4, $5, $6)").await {
                    Ok(stmt) => match connection.execute(&stmt, &[&channel.id, &channel.creator, &channel.deposit_asset, &channel.deposit_amount, &channel.valid_until, &channel.spec]).await {
                        Ok(row) => {
                            let inserted = row == 1;
                            Ok((inserted, connection))
                        },
                        Err(e) => Err((e, connection)),
                    },
                    Err(e) => Err((e, connection)),
                }
            }
        })
        .await
}
