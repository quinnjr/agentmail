//! agentmail: mail and blocking questions between coding agents and a human.

pub mod client;
pub mod daemon;
pub mod ids;
pub mod model;
pub mod paths;
pub mod procinfo;
pub mod protocol;
pub mod store;
#[cfg(test)]
mod test_util;
