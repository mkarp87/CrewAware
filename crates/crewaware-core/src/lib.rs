//! Pure logic for CrewAware: model, contact rules, geometry and the in-memory engine.
//! No I/O lives here, so every rule can be unit tested.

pub mod engine;
pub mod geo;
pub mod model;
pub mod route;

pub use engine::{Candidate, Change, Engine, EngineError, Snapshot};
pub use model::*;
pub use route::{Route, RouteField};
