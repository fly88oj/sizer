//! Parsed form of a Sizer macro.

/// Position in the macro text, 1-based line and column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: u32,
    pub column: u32,
    pub offset: usize,
}

impl core::fmt::Display for Span {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "line {}, column {}", self.line, self.column)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{} at {}", self.message, self.span)
    }
}

impl std::error::Error for ParseError {}

/// One macro: optional settings, variables and `resize` blocks, in source order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Macro {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Option {
        option: MacroOption,
        enabled: bool,
        span: Span,
    },
    Assign {
        name: String,
        value: Expr,
        span: Span,
    },
    Resize(ResizeBlock),
}

/// `option <name> on|off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroOption {
    /// `compensate_frame_effects`
    CompensateFrameEffects,
    /// `ignore_runtime_errors`
    IgnoreRuntimeErrors,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResizeBlock {
    /// Filters from the window qualifier; empty means "the active window".
    pub filters: Vec<Filter>,
    /// Trailing `*`: every matching window is processed, not only the first.
    pub all: bool,
    pub body: Vec<Action>,
    pub span: Span,
}

impl ResizeBlock {
    /// Whether the block names a window at all (`resize` with no qualifier).
    pub fn has_filter(&self) -> bool {
        !self.filters.is_empty()
    }
}

/// One term of a window qualifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    Last,
    Active,
    Proc {
        text: String,
        desktop: bool,
        index: Option<u32>,
    },
    Cap {
        text: String,
        desktop: bool,
        index: Option<u32>,
    },
}

/// A `resize` body entry.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Assign {
        axis: Axis,
        value: Expr,
        span: Span,
    },
    Variable {
        name: String,
        value: Expr,
        span: Span,
    },
    Restore {
        span: Span,
    },
    Minimize {
        span: Span,
    },
    Maximize {
        monitor: Option<MonitorRef>,
        span: Span,
    },
}

/// The six dimension letters. `Right`/`Bottom` are readable but, as in Sizer,
/// not assignable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Axis {
    Top,
    Left,
    Right,
    Bottom,
    Width,
    Height,
}

impl Axis {
    pub fn letter(self) -> char {
        match self {
            Axis::Top => 't',
            Axis::Left => 'l',
            Axis::Right => 'r',
            Axis::Bottom => 'b',
            Axis::Width => 'w',
            Axis::Height => 'h',
        }
    }

    pub fn from_letter(letter: &str) -> Option<Self> {
        match letter {
            "t" => Some(Axis::Top),
            "l" => Some(Axis::Left),
            "r" => Some(Axis::Right),
            "b" => Some(Axis::Bottom),
            "w" => Some(Axis::Width),
            "h" => Some(Axis::Height),
            _ => None,
        }
    }

    pub fn is_assignable(self) -> bool {
        matches!(self, Axis::Top | Axis::Left | Axis::Width | Axis::Height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
    IntDiv,
    Rem,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Number(f64),
    Var(String),
    Dimension(Dimension),
    Binary {
        op: Op,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Negate(Box<Expr>),
    /// A leading `+`; kept so `+x` parses exactly as the diagram allows.
    Positive(Box<Expr>),
}

/// `w:mon`, `h:client:proc[x]`, `t` and so on.
#[derive(Debug, Clone, PartialEq)]
pub struct Dimension {
    pub axis: Axis,
    pub area: DimArea,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DimArea {
    /// No suffix: the same basis the resize block uses.
    Default,
    Client(Option<WindowSpec>),
    Win(Option<WindowSpec>),
    Mon(Option<MonitorRef>),
    WorkArea(Option<MonitorRef>),
}

/// A window qualifier used as a dimension suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSpec {
    pub filters: Vec<Filter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorRef {
    Primary,
    /// `current`, and `active` as its documented synonym.
    Current,
    Index(u32),
}
