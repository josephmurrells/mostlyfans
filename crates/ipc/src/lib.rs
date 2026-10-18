#![cfg(windows)]

mod client;
mod pipe;
mod server;

pub use client::{Client, ClientError, ServerCheck};
pub use pipe::PipeStream;
pub use server::PipeListener;
