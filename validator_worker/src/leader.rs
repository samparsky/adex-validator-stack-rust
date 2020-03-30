use std::error::Error;

use primitives::adapter::{Adapter, AdapterErrorKind};
use primitives::validator::{Accounting, MessageTypes, NewState};
use primitives::BalancesMap;

use crate::heartbeat::{heartbeat, HeartbeatStatus};
use crate::sentry_interface::{PropagationResult, SentryApi};
use crate::{get_state_root_hash, producer};

#[derive(Debug)]
pub enum NewStateResult<AE: AdapterErrorKind> {
    Sent(Vec<PropagationResult<AE>>),
    /// Conditions for sending the new state haven't been met
    NotSent,
}

#[derive(Debug)]
pub struct TickStatus<AE: AdapterErrorKind> {
    pub heartbeat: HeartbeatStatus<AE>,
    /// If None, then the conditions for handling a new state haven't been met
    pub new_state: NewStateResult<AE>,
    pub producer_tick: producer::TickStatus<AE>,
}

pub async fn tick<A: Adapter + 'static>(
    iface: &SentryApi<A>,
) -> Result<TickStatus<A::AdapterError>, Box<dyn Error>> {
    let producer_tick = producer::tick(&iface).await?;
    let (balances, new_state) = match &producer_tick {
        producer::TickStatus::Sent {
            balances,
            new_accounting,
            ..
        } => {
            let new_state = on_new_accounting(&iface, (balances, new_accounting)).await?;
            (balances, new_state)
        }
        producer::TickStatus::NoNewEventAggr(balances) => (balances, NewStateResult::NotSent),
    };

    Ok(TickStatus {
        heartbeat: heartbeat(&iface, &balances).await?,
        new_state,
        producer_tick,
    })
}

async fn on_new_accounting<A: Adapter + 'static>(
    iface: &SentryApi<A>,
    (balances, new_accounting): (&BalancesMap, &Accounting),
) -> Result<NewStateResult<A::AdapterError>, Box<dyn Error>> {
    let state_root_raw = get_state_root_hash(&iface, &balances)?;
    let state_root = hex::encode(state_root_raw);

    let signature = iface.adapter.sign(&state_root)?;

    let propagation_results = iface
        .propagate(&[&MessageTypes::NewState(NewState {
            state_root,
            signature,
            balances: new_accounting.balances.clone(),
        })])
        .await;

    Ok(NewStateResult::Sent(propagation_results))
}
