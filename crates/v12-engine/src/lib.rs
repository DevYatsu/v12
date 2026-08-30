#![forbid(unsafe_code)]

//! Engine core: realms, internal methods, built-in objects, job queue, and the
//! embedding interface for the v12 JavaScript engine.

pub mod builtins;
pub mod engine;
pub mod error;
pub mod internal_methods;
pub mod job_queue;
pub mod realm;
pub mod value;

pub use engine::Engine;
pub use error::EngineError;
pub use job_queue::JobQueue;
pub use realm::Realm;
pub use value::{FromValue, ToValue};
pub use builtins::HostClosure;
pub use v12_native::{NativeId, Throw};

pub use v12_heap::{Heap, JsValue};

#[cfg(test)]
mod tests;
