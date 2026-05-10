pub mod server;
pub mod handlers;

// Re-export IpcContext for use in lib.rs setup
pub use server::IpcContext;
