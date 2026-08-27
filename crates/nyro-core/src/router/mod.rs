pub mod decision;
pub mod health;
pub mod latency;
mod matcher;
pub mod quota;
pub mod selector;
pub(crate) mod usage;

pub use latency::LatencyRegistry;
pub use matcher::ModelCache;
pub use selector::{
    LatencyStrategy, PriorityStrategy, RoutingStrategy, SelectedTarget, TargetSelector,
    UsageStrategy, WeightedStrategy,
};

use crate::db::models::Model;

impl ModelCache {
    pub fn match_model(&self, model: &str) -> Option<&Model> {
        matcher::match_model(&self.models, model)
    }
}
