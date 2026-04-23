#![deny(warnings)]

mod grammar;
pub use grammar::{Grammar, GrammarBuilder};

mod parser;
mod spans;
pub use parser::EarleyParser;

mod trees;
pub use trees::{EarleyForest, ForestWalkError};

#[cfg(test)]
mod parser_test;
