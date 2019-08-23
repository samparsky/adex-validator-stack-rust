#![deny(rust_2018_idioms)]
#![deny(clippy::all)]
use std::error;
use std::fmt;

pub mod big_num;
pub mod config;
pub mod adapter;
pub mod channel;
pub mod channel_validator;
pub mod util;
pub mod validator;
pub mod targeting_tag;
pub mod event_submission;
pub mod ad_unit;
pub mod balances_map;

//#[cfg(any(test, feature = "fixtures"))]
//pub use util::tests as test_util;
//
//pub use self::asset::Asset;
pub use self::balances_map::BalancesMap;
pub use self::ad_unit::AdUnit;
pub use self::big_num::BigNum;
pub use self::config::Config;
pub use self::channel::{Channel, ChannelSpec, SpecValidator, SpecValidators};
pub use self::validator::{ValidatorDesc};
pub use self::event_submission::EventSubmission;
//#[cfg(feature = "repositories")]
//pub use self::repository::*;
pub use self::targeting_tag::TargetingTag;
////pub use self::validator::{ValidatorDesc, ValidatorId};
//
//pub mod asset;

//pub mod channel;
//pub mod targeting_tag;
//pub mod validator;
//
///// re-exports all the fixtures in one module
//#[cfg(any(test, feature = "fixtures"))]
//pub mod fixtures {
//    pub use super::asset::fixtures::*;
//    pub use super::channel::fixtures::*;
//    pub use super::targeting_tag::fixtures::*;
////    pub use super::validator::fixtures::*;
//}
//
#[derive(Debug)]
pub enum DomainError {
   InvalidArgument(String),
}

impl fmt::Display for DomainError {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
       write!(f, "Domain error",)
   }
}

impl error::Error for DomainError {
   fn cause(&self) -> Option<&dyn error::Error> {
       None
   }
}
