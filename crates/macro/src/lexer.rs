//! Tokens for the Sizer macro language, each carrying its source position.

use crate::ast::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// Keyword or bare word, also used for the letters `t l r b w h`.
    Ident(String),
    /// Decimal literal; the grammar has no exponent and no sign.
    Number(f64),
    /// `$name`, including the leading dollar sign.
    Variable(String),
    /// Contents of `[...]`, with any surrounding quotes removed.
    Text(String),
    Colon,
    Star,
    Equals,
    Plus,
    Minus,
    Slash,
    Backslash,
    Percent,
    LParen,
    RParen,
    Eof,
}

impl Token {
    /// How the token is written, for "unexpected token" messages.
    pub fn describe(&self) -> String {
        match self {
            Token::Ident(text) => format!("`{text}`"),
            Token::Number(value) => format!("`{value}`"),
            Token::Variable(name) => format!("`{name}`"),
            Token::Text(text) => format!("`[{text}]`"),
            Token::Colon => "`:`".to_string(),
            Token::Star => "`*`".to_string(),
            Token::Equals => "`=`".to_string(),
            Token::Plus => "`+`".to_string(),
            Token::Minus => "`-`".to_string(),
            Token::Slash => "`/`".to_string(),
            Token::Backslash => "`\\`".to_string(),
            Token::Percent => "`%`".to_string(),
            Token::LParen => "`(`".to_string(),
            Token::RParen => "`)`".to_string(),
            Token::Eof => "end of macro".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    pub token: Token,
    pub span: Span,
}

/// Why the source could not be turned into tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub message: String,
    pub span: Span,
}

impl core::fmt::Display for LexError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{} at {}", self.message, self.span)
    }
}

pub fn lex(source: &str) -> Result<Vec<Spanned>, LexError> {
    Lexer::new(source).run()
}

struct Lexer<'a> {
    source: &'a str,
    bytes: &'a [u8],
    offset: usize,
    line: u32,
    column: u32,
    tokens: Vec<Spanned>,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            offset: 0,
            line: 1,
            column: 1,
            tokens: Vec::new(),
        }
    }

    fn span(&self) -> Span {
        Span {
            line: self.line,
            column: self.column,
            offset: self.offset,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }

    fn bump(&mut self) {
        if let Some(byte) = self.peek() {
            self.offset += 1;
            if byte == b'\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
        }
    }

    fn run(mut self) -> Result<Vec<Spanned>, LexError> {
        loop {
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                self.bump();
            }
            let Some(byte) = self.peek() else {
                break;
            };
            let span = self.span();
            let token = match byte {
                b':' => {
                    self.bump();
                    Token::Colon
                }
                b'*' => {
                    self.bump();
                    Token::Star
                }
                b'=' => {
                    self.bump();
                    Token::Equals
                }
                b'+' => {
                    self.bump();
                    Token::Plus
                }
                b'-' => {
                    self.bump();
                    Token::Minus
                }
                b'/' => {
                    self.bump();
                    Token::Slash
                }
                b'\\' => {
                    self.bump();
                    Token::Backslash
                }
                b'%' => {
                    self.bump();
                    Token::Percent
                }
                b'(' => {
                    self.bump();
                    Token::LParen
                }
                b')' => {
                    self.bump();
                    Token::RParen
                }
                b'[' => self.text(span)?,
                b'$' => self.variable(span)?,
                b'0'..=b'9' => self.number(span)?,
                byte if is_word_byte(byte) => self.word(),
                other => {
                    return Err(LexError {
                        message: format!("unexpected character `{}`", other as char),
                        span,
                    });
                }
            };
            self.tokens.push(Spanned { token, span });
        }
        self.tokens.push(Spanned {
            token: Token::Eof,
            span: self.span(),
        });
        Ok(self.tokens)
    }

    fn word(&mut self) -> Token {
        let start = self.offset;
        while matches!(self.peek(), Some(byte) if is_word_byte(byte)) {
            self.bump();
        }
        Token::Ident(self.source[start..self.offset].to_string())
    }

    fn variable(&mut self, span: Span) -> Result<Token, LexError> {
        self.bump(); // $
        let start = self.offset;
        while matches!(self.peek(), Some(byte) if is_word_byte(byte)) {
            self.bump();
        }
        if start == self.offset {
            return Err(LexError {
                message: "`$` must be followed by a variable name".to_string(),
                span,
            });
        }
        Ok(Token::Variable(format!(
            "${}",
            &self.source[start..self.offset]
        )))
    }

    fn number(&mut self, span: Span) -> Result<Token, LexError> {
        let start = self.offset;
        while matches!(self.peek(), Some(byte) if byte.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some(b'.') {
            self.bump();
            let fraction = self.offset;
            while matches!(self.peek(), Some(byte) if byte.is_ascii_digit()) {
                self.bump();
            }
            if fraction == self.offset {
                return Err(LexError {
                    message: "a decimal point must be followed by digits".to_string(),
                    span,
                });
            }
        }
        let text = &self.source[start..self.offset];
        Ok(Token::Number(text.parse().map_err(|_| LexError {
            message: format!("`{text}` is not a number"),
            span,
        })?))
    }

    /// A `[...]` window-filter text. The diagram leaves the quoting rules
    /// undocumented, so both `[notepad]` and `["My App"]` are accepted, with
    /// backslash escapes only inside quotes.
    fn text(&mut self, span: Span) -> Result<Token, LexError> {
        self.bump(); // [
        let mut quoted = false;
        let mut value = String::new();
        if self.peek() == Some(b'"') {
            quoted = true;
            self.bump();
        }
        loop {
            match self.peek() {
                None => {
                    return Err(LexError {
                        message: "unterminated `[` in window filter".to_string(),
                        span,
                    });
                }
                Some(b']') if !quoted => {
                    self.bump();
                    break;
                }
                Some(b'"') if quoted => {
                    self.bump();
                    if self.peek() == Some(b']') {
                        self.bump();
                        break;
                    }
                    return Err(LexError {
                        message: "expected `]` after the closing quote".to_string(),
                        span,
                    });
                }
                Some(b'\\') if quoted => {
                    self.bump();
                    value.push(self.peek().map(|byte| byte as char).unwrap_or('\\'));
                    self.bump();
                }
                Some(b'\n') if quoted => {
                    return Err(LexError {
                        message: "unterminated quoted window filter".to_string(),
                        span,
                    });
                }
                Some(byte) => {
                    value.push(byte as char);
                    self.bump();
                }
            }
        }
        Ok(Token::Text(if quoted {
            value
        } else {
            value.trim().to_string()
        }))
    }
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}
