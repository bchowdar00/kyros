//! The strategy runtime: the interpreter over compiled programs, the
//! per-deployment state, the deployment task that runs an action batch per
//! activation against a [`crate::automation::venue::Venue`], and the manager
//! that owns every deployment.

pub mod deployment;
pub mod interpreter;
pub mod manager;
pub mod risk;
pub mod state;

pub use deployment::{DeploymentSpec, DeploymentStatus, LogicalOrder, next_deploy_epoch};
pub use manager::{RuntimeCommand, RuntimeHandle, StartError, runtime_channel, spawn_manager};

#[cfg(test)]
mod interpreter_tests;
#[cfg(test)]
mod runtime_tests;
#[cfg(test)]
mod behaviour_tests;
#[cfg(test)]
mod engine_venue_tests;
#[cfg(test)]
pub(crate) mod test_support;
