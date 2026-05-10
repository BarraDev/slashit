mod state;
mod parser;
mod input;
mod render;
mod component;

pub use component::RealTerminal;
pub use state::{TerminalState, Cell, CellAttributes};
pub use parser::parse_bytes;

#[cfg(test)]
mod tests;
