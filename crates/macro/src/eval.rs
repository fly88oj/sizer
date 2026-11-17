//! Expression evaluation.
//!
//! This is pure arithmetic: the caller supplies the dimension values for the
//! window being processed, which keeps the X11 backend and the GNOME extension
//! path on the same evaluator.

use std::collections::HashMap;

use crate::ast::{Axis, DimArea, Expr, Op};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalError {
    pub message: String,
}

impl core::fmt::Display for EvalError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EvalError {}

/// Supplies the six dimensions of a window, monitor or work area.
pub trait Facts {
    fn dimension(&self, axis: Axis, area: &DimArea) -> Result<f64, EvalError>;
}

pub fn eval(
    expression: &Expr,
    variables: &HashMap<String, f64>,
    facts: &dyn Facts,
) -> Result<f64, EvalError> {
    match expression {
        Expr::Number(value) => Ok(*value),
        Expr::Var(name) => variables.get(name).copied().ok_or_else(|| EvalError {
            message: format!("variable `{name}` is used before it is assigned"),
        }),
        Expr::Dimension(dimension) => facts.dimension(dimension.axis, &dimension.area),
        Expr::Negate(inner) => Ok(-eval(inner, variables, facts)?),
        Expr::Positive(inner) => eval(inner, variables, facts),
        Expr::Binary { op, left, right } => {
            let left = eval(left, variables, facts)?;
            let right = eval(right, variables, facts)?;
            match op {
                Op::Add => Ok(left + right),
                Op::Sub => Ok(left - right),
                Op::Mul => Ok(left * right),
                Op::Div => {
                    if right == 0.0 {
                        return Err(EvalError {
                            message: "division by zero".to_string(),
                        });
                    }
                    Ok(left / right)
                }
                // `\` is integer division; Sizer documents it as floor, so
                // negative operands round toward minus infinity, not toward zero.
                Op::IntDiv => {
                    if right == 0.0 {
                        return Err(EvalError {
                            message: "division by zero".to_string(),
                        });
                    }
                    Ok((left / right).floor())
                }
                Op::Rem => {
                    if right == 0.0 {
                        return Err(EvalError {
                            message: "division by zero".to_string(),
                        });
                    }
                    Ok(left % right)
                }
            }
        }
    }
}
