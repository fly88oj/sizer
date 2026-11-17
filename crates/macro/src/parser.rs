//! Recursive-descent parser for the Sizer 4 macro language.
//!
//! The productions below are transcribed from the railroad diagram at
//! <https://www.brianapps.net/sizer4/rail.html> (the diagram is rendered by
//! `RailroadLoader.astro_...js`, which is what fixes the terminals):
//!
//! ```text
//! macro            := ( option | variable | resize-block )*
//! option           := "option" ( "compensate_frame_effects" | "ignore_runtime_errors" )
//!                     ( "on" | "off" )
//! resize-block     := "resize" [ window-qualifier [ "*" ] ]
//!                     ( assignment | variable | "restore" | "minimize"
//!                       | "maximize" [ monitor-qualifier ] )*
//! assignment       := ( "t" | "l" | "w" | "h" ) "=" expression
//! variable         := variable-name "=" expression
//! variable-name    := "$" ( "0…9" | "a…z" | "A…Z" )+
//! expression       := factor ( ( "+" | "-" ) factor )*
//! factor           := unary ( ( "*" | "/" | "\" | "%" ) unary )*
//! unary            := [ "+" | "-" ] term
//! term             := dimension | variable-name | number | "(" expression ")"
//! dimension        := ( "t" | "l" | "r" | "b" | "w" | "h" )
//!                     [ ":" ( ( "client" | "win" ) [ window-qualifier ]
//!                           | ( "mon" | "workarea" ) [ monitor-qualifier ] ) ]
//! monitor-qualifier:= ":" ( "primary" | "current" | number )
//! window-qualifier := ":" ( "last" | "active"
//!                         | ( "proc" | "cap" ) "[" text "]" [ ":" "desktop" ]
//!                           [ ":" number ] ( ":" ( "proc" | "cap" ) ... )* )
//! number           := digits [ "." digits ]
//! ```
//!
//! Two points the diagram leaves open, decided here and marked in the code:
//! the diagram joins repeated `proc`/`cap` filters with `:`, so that colon is
//! consumed as a separator rather than as the start of a `desktop`/index suffix
//! when it is followed by another filter; and the quoting rules of `[text]` are
//! undocumented, so both `[notepad]` and `["My App"]` are accepted.

use crate::ast::{
    Action, Axis, DimArea, Dimension, Expr, Filter, Item, Macro, MacroOption, MonitorRef, Op,
    ParseError, ResizeBlock, Span, WindowSpec,
};
use crate::lexer::{Spanned, Token, lex};

pub fn parse(source: &str) -> Result<Macro, ParseError> {
    let tokens = lex(source).map_err(|error| ParseError {
        message: error.message,
        span: error.span,
    })?;
    let mut parser = Parser { tokens, index: 0 };
    parser.macro_body()
}

struct Parser {
    tokens: Vec<Spanned>,
    index: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.index].token
    }

    fn peek_at(&self, ahead: usize) -> &Token {
        let index = (self.index + ahead).min(self.tokens.len() - 1);
        &self.tokens[index].token
    }

    fn span(&self) -> Span {
        self.tokens[self.index].span
    }

    fn advance(&mut self) -> Spanned {
        let token = self.tokens[self.index].clone();
        if self.index + 1 < self.tokens.len() {
            self.index += 1;
        }
        token
    }

    fn error<T>(&self, message: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError {
            message: message.into(),
            span: self.span(),
        })
    }

    /// Sizer's own wording for stray tokens, with the position appended.
    fn unexpected<T>(&self) -> Result<T, ParseError> {
        self.error(format!(
            "Unexpected token in macro: {}",
            self.peek().describe()
        ))
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == token {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: &Token, what: &str) -> Result<Span, ParseError> {
        if self.peek() == token {
            Ok(self.advance().span)
        } else {
            self.error(format!("expected {what}, found {}", self.peek().describe()))
        }
    }

    fn macro_body(&mut self) -> Result<Macro, ParseError> {
        let mut items = Vec::new();
        loop {
            match self.peek() {
                Token::Eof => break,
                Token::Ident(word) if word == "option" => items.push(self.option()?),
                Token::Ident(word) if word == "resize" => items.push(Item::Resize(self.resize()?)),
                Token::Variable(_) => items.push(self.variable()?),
                _ => return self.unexpected(),
            }
        }
        Ok(Macro { items })
    }

    fn option(&mut self) -> Result<Item, ParseError> {
        let span = self.advance().span; // option
        let option = match self.peek() {
            Token::Ident(word) if word == "compensate_frame_effects" => {
                self.advance();
                MacroOption::CompensateFrameEffects
            }
            Token::Ident(word) if word == "ignore_runtime_errors" => {
                self.advance();
                MacroOption::IgnoreRuntimeErrors
            }
            _ => return self.error(format!("unknown macro option {}", self.peek().describe())),
        };
        let enabled = match self.peek() {
            Token::Ident(word) if word == "on" => {
                self.advance();
                true
            }
            Token::Ident(word) if word == "off" => {
                self.advance();
                false
            }
            _ => return self.error("an option needs `on` or `off`"),
        };
        Ok(Item::Option {
            option,
            enabled,
            span,
        })
    }

    fn variable(&mut self) -> Result<Item, ParseError> {
        let span = self.span();
        let Token::Variable(name) = self.advance().token else {
            return self.unexpected();
        };
        self.expect(&Token::Equals, "`=` after the variable name")?;
        let value = self.expression()?;
        Ok(Item::Assign { name, value, span })
    }

    fn resize(&mut self) -> Result<ResizeBlock, ParseError> {
        let span = self.advance().span; // resize
        let mut filters = Vec::new();
        if self.peek() == &Token::Colon {
            filters = self.window_spec()?;
        }
        let all = self.eat(&Token::Star);
        let mut body = Vec::new();
        loop {
            match self.peek() {
                Token::Ident(word) if word == "restore" => {
                    let span = self.advance().span;
                    body.push(Action::Restore { span });
                }
                Token::Ident(word) if word == "minimize" => {
                    let span = self.advance().span;
                    body.push(Action::Minimize { span });
                }
                Token::Ident(word) if word == "maximize" => {
                    let span = self.advance().span;
                    let monitor = if self.peek() == &Token::Colon {
                        Some(self.monitor_ref()?)
                    } else {
                        None
                    };
                    body.push(Action::Maximize { monitor, span });
                }
                Token::Ident(word) if Axis::from_letter(word).is_some() => {
                    let span = self.span();
                    let axis = Axis::from_letter(word).expect("checked above");
                    self.advance();
                    if !axis.is_assignable() {
                        return self.error(format!(
                            "`{}` cannot be assigned; only t, l, w and h can",
                            axis.letter()
                        ));
                    }
                    self.expect(&Token::Equals, "`=` after the dimension")?;
                    let value = self.expression()?;
                    body.push(Action::Assign { axis, value, span });
                }
                Token::Variable(_) => match self.variable()? {
                    Item::Assign { name, value, span } => {
                        body.push(Action::Variable { name, value, span });
                    }
                    _ => unreachable!("variable() only returns Item::Assign"),
                },
                _ => break,
            }
        }
        Ok(ResizeBlock {
            filters,
            all,
            body,
            span,
        })
    }

    /// `: last | active | proc[...] | cap[...]`, with `:` joining repeats.
    fn window_spec(&mut self) -> Result<Vec<Filter>, ParseError> {
        self.expect(&Token::Colon, "`:` before the window filter")?;
        let mut filters = vec![self.filter()?];
        while self.peek() == &Token::Colon && self.starts_filter() {
            self.advance(); // separator between repeated filters
            filters.push(self.filter()?);
        }
        Ok(filters)
    }

    fn starts_filter(&self) -> bool {
        matches!(self.peek_at(1), Token::Ident(word) if matches!(word.as_str(), "last" | "active" | "proc" | "cap"))
    }

    fn filter(&mut self) -> Result<Filter, ParseError> {
        let word = match self.peek() {
            Token::Ident(word) => word.clone(),
            _ => return self.filter_error(),
        };
        match word.as_str() {
            "last" => {
                self.advance();
                Ok(Filter::Last)
            }
            "active" => {
                self.advance();
                Ok(Filter::Active)
            }
            "proc" | "cap" => {
                self.advance();
                let text = match self.peek() {
                    Token::Text(text) => {
                        let text = text.clone();
                        self.advance();
                        text
                    }
                    _ => return self.error(format!("expected `[text]` after `{word}`")),
                };
                let mut desktop = false;
                let mut index = None;
                // A `:` here can start `desktop`, an index, or the next filter;
                // only the first two belong to this filter.
                if self.peek() == &Token::Colon {
                    match self.peek_at(1) {
                        Token::Ident(word) if word == "desktop" => {
                            self.advance();
                            self.advance();
                            desktop = true;
                        }
                        Token::Number(_) => {
                            self.advance();
                            index = Some(self.index_number()?);
                        }
                        _ => {}
                    }
                }
                if self.peek() == &Token::Colon {
                    if let Token::Number(_) = self.peek_at(1) {
                        self.advance();
                        index = Some(self.index_number()?);
                    }
                }
                if word == "proc" {
                    Ok(Filter::Proc {
                        text,
                        desktop,
                        index,
                    })
                } else {
                    Ok(Filter::Cap {
                        text,
                        desktop,
                        index,
                    })
                }
            }
            _ => self.filter_error(),
        }
    }

    /// Sizer's own message for a bad window filter.
    fn filter_error<T>(&self) -> Result<T, ParseError> {
        self.error(format!(
            "Expecting 'cap', 'proc', 'desktop' or number in window filter, found {}",
            self.peek().describe()
        ))
    }

    fn index_number(&mut self) -> Result<u32, ParseError> {
        let span = self.span();
        match self.advance().token {
            Token::Number(value) if value >= 0.0 && value.fract() == 0.0 => Ok(value as u32),
            Token::Number(value) => Err(ParseError {
                message: format!("expected a whole number, found {value}"),
                span,
            }),
            other => Err(ParseError {
                message: format!("expected a number, found {}", other.describe()),
                span,
            }),
        }
    }

    fn monitor_ref(&mut self) -> Result<MonitorRef, ParseError> {
        self.expect(&Token::Colon, "`:` before the monitor")?;
        match self.peek() {
            Token::Ident(word) if word == "primary" => {
                self.advance();
                Ok(MonitorRef::Primary)
            }
            // `current` is the diagram's terminal; `active` is its documented
            // synonym ("current/pointer monitor"), accepted here on purpose.
            Token::Ident(word) if word == "current" || word == "active" => {
                self.advance();
                Ok(MonitorRef::Current)
            }
            Token::Number(_) => {
                // Kept exactly as written; the executor decides whether the
                // first monitor is 0 or 1 in its platform model.
                let index = self.index_number()?;
                Ok(MonitorRef::Index(index))
            }
            _ => self.error(format!(
                "expected `primary`, `current` or a monitor index, found {}",
                self.peek().describe()
            )),
        }
    }

    fn expression(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.factor()?;
        loop {
            let op = match self.peek() {
                Token::Plus => Op::Add,
                Token::Minus => Op::Sub,
                _ => break,
            };
            self.advance();
            let right = self.factor()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn factor(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => Op::Mul,
                Token::Slash => Op::Div,
                Token::Backslash => Op::IntDiv,
                Token::Percent => Op::Rem,
                _ => break,
            };
            self.advance();
            let right = self.unary()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, ParseError> {
        match self.peek() {
            Token::Minus => {
                self.advance();
                Ok(Expr::Negate(Box::new(self.unary()?)))
            }
            Token::Plus => {
                self.advance();
                Ok(Expr::Positive(Box::new(self.unary()?)))
            }
            _ => self.term(),
        }
    }

    fn term(&mut self) -> Result<Expr, ParseError> {
        match self.peek().clone() {
            Token::Number(value) => {
                self.advance();
                Ok(Expr::Number(value))
            }
            Token::Variable(name) => {
                self.advance();
                Ok(Expr::Var(name))
            }
            Token::LParen => {
                self.advance();
                let inner = self.expression()?;
                self.expect(&Token::RParen, "`)`")?;
                Ok(inner)
            }
            Token::Ident(word) => {
                let Some(axis) = Axis::from_letter(&word) else {
                    return self.unexpected();
                };
                self.advance();
                let area = if self.peek() == &Token::Colon {
                    self.dimension_suffix()?
                } else {
                    DimArea::Default
                };
                Ok(Expr::Dimension(Dimension { axis, area }))
            }
            _ => self.unexpected(),
        }
    }

    fn dimension_suffix(&mut self) -> Result<DimArea, ParseError> {
        self.expect(&Token::Colon, "`:`")?;
        match self.peek() {
            Token::Ident(word) if word == "client" || word == "win" => {
                let word = word.clone();
                self.advance();
                let spec = if self.peek() == &Token::Colon {
                    Some(WindowSpec {
                        filters: self.window_spec()?,
                    })
                } else {
                    None
                };
                if word == "client" {
                    Ok(DimArea::Client(spec))
                } else {
                    Ok(DimArea::Win(spec))
                }
            }
            Token::Ident(word) if word == "mon" || word == "workarea" => {
                let word = word.clone();
                self.advance();
                let monitor = if self.peek() == &Token::Colon {
                    Some(self.monitor_ref()?)
                } else {
                    None
                };
                if word == "mon" {
                    Ok(DimArea::Mon(monitor))
                } else {
                    Ok(DimArea::WorkArea(monitor))
                }
            }
            _ => self.error(format!(
                "expected `client`, `win`, `mon` or `workarea` after `:`, found {}",
                self.peek().describe()
            )),
        }
    }
}
