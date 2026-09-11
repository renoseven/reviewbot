//! The wire protocol for model calls. `Request` and `Response` are the only
//! shapes anything above this layer sees; vendor JSON stops here.
//!
//! One of the three extension points.

pub mod error;
pub mod openai;
pub mod types;

pub use error::ProtocolError;
pub use openai::OpenAi;
pub use types::{InputItem, OutputItem, Request, Response, Role, ToolSchema};

pub trait Protocol: Send + Sync {
    fn name(&self) -> &'static str;

    fn send(&self, request: &Request) -> Result<Response, ProtocolError>;
}
