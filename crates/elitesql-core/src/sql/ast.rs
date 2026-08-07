//! AST for the deliberately small EliteSQL SQL dialect (V1 subset).

#[derive(Debug, Clone)]
pub(crate) enum Statement {
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
    },
    CreateIndex {
        table: String,
        column: String,
        unique: bool,
    },
    Insert {
        table: String,
        columns: Vec<String>,
        rows: Vec<Vec<Literal>>,
    },
    Select(Box<SelectStmt>),
    Update {
        table: String,
        sets: Vec<(String, Literal)>,
        where_clause: Option<Expr>,
    },
    Delete {
        table: String,
        where_clause: Option<Expr>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ColumnDef {
    pub name: String,
    pub ty: crate::value::ColumnType,
    pub not_null: bool,
    /// Dimension for `vector(N)` columns.
    pub dim: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct SelectStmt {
    pub projection: Vec<SelectItem>,
    pub from: TableRef,
    pub joins: Vec<Join>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<ColumnRef>,
    pub having: Option<Expr>,
    pub order_by: Vec<(ColumnRef, bool)>, // bool = descending
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFunc {
    pub fn name(&self) -> &'static str {
        match self {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Avg => "avg",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum SelectItem {
    Star,
    Column {
        col: ColumnRef,
        alias: Option<String>,
    },
    Aggregate {
        func: AggFunc,
        /// None = COUNT(*)
        arg: Option<ColumnRef>,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct TableRef {
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinKind {
    Inner,
    Left,
    Right,
}

#[derive(Debug, Clone)]
pub(crate) struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    /// V1 supports exactly one equality: ON a.x = b.y
    pub on: (ColumnRef, ColumnRef),
}

#[derive(Debug, Clone)]
pub(crate) struct ColumnRef {
    pub table: Option<String>,
    pub column: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Literal {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Blob(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmpOp {
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone)]
pub(crate) enum Operand {
    Col(ColumnRef),
    Lit(Literal),
    /// Aggregate call; only legal in HAVING.
    Agg { func: AggFunc, arg: Option<ColumnRef> },
}

#[derive(Debug, Clone)]
pub(crate) enum Expr {
    Cmp {
        left: Operand,
        op: CmpOp,
        right: Operand,
    },
    IsNull {
        col: ColumnRef,
        negated: bool,
    },
    InList {
        col: ColumnRef,
        list: Vec<Literal>,
        negated: bool,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}
