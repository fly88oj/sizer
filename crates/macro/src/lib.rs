//! Sizer 4 macro language: lexer, parser and expression evaluator.
//!
//! The grammar is the one published as a railroad diagram at
//! <https://www.brianapps.net/sizer4/rail.html>; the productions are reproduced
//! in [`parser`] next to the code that implements them. [`exec`] runs a parsed
//! macro against a [`WindowHost`] supplied by each platform, which keeps the X11
//! backend and the GNOME extension on the same selector and evaluator.

mod ast;
mod eval;
mod exec;
#[cfg(test)]
mod exec_tests;
mod lexer;
mod parser;
#[cfg(test)]
mod tests;

pub use ast::{
    Action, Axis, DimArea, Dimension, Expr, Filter, Item, Macro, MacroOption, MonitorRef, Op,
    ParseError, ResizeBlock, Span, WindowSpec,
};
pub use eval::{EvalError, Facts, eval};
pub use exec::{
    ExecError, MonitorSnapshot, Outcome, Report, Skip, WindowGeometry, WindowHost, WindowSnapshot,
    execute,
};
pub use parser::parse;
