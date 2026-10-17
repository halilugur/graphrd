//! SPARQL 1.1 query/update syntax trees, already in algebra form.

use crate::rdf::Term;

/// Variables are resolved to slots in a per-query table.
pub type Var = usize;

#[derive(Debug, Clone, PartialEq)]
/// A subject or object position of a pattern: a variable, a term, or a quoted triple.
pub enum VarOrTerm {
    /// A variable slot.
    Var(Var),
    /// A constant term.
    Term(Term),
    /// RDF-star quoted triple pattern with at least one variable.
    Triple(Box<[VarOrTerm; 3]>),
}

impl VarOrTerm {
    /// Push every variable of this position into `out`, in order.
    pub fn vars(&self, out: &mut Vec<Var>) {
        match self {
            VarOrTerm::Var(v) => out.push(*v),
            VarOrTerm::Term(_) => {}
            VarOrTerm::Triple(t) => t.iter().for_each(|x| x.vars(out)),
        }
    }
}

#[derive(Debug, Clone)]
/// A property path.
pub enum Path {
    /// A single IRI.
    Iri(String),
    /// `^p`: the path walked backwards.
    Inverse(Box<Path>),
    /// `p1/p2`: one path after the other.
    Seq(Vec<Path>),
    /// `p1|p2`: either path.
    Alt(Vec<Path>),
    /// `p*`: zero or more steps.
    ZeroOrMore(Box<Path>),
    /// `p+`: one or more steps.
    OneOrMore(Box<Path>),
    /// `p?`: an optional step.
    ZeroOrOne(Box<Path>),
    /// !(a|^b): (iri, inverse)
    Negated(Vec<(String, bool)>),
}

#[derive(Debug, Clone)]
/// The predicate position of a triple pattern.
pub enum Predicate {
    /// The predicate is a variable slot.
    Var(Var),
    /// The predicate is an IRI or a property path.
    Path(Path),
}

#[derive(Debug, Clone)]
/// One triple pattern: subject, predicate and object positions.
pub struct TriplePattern {
    /// The subject: variable, term, or quoted triple pattern.
    pub s: VarOrTerm,
    /// The predicate: variable, IRI, or path.
    pub p: Predicate,
    /// The object: variable, term, or quoted triple pattern.
    pub o: VarOrTerm,
}

#[derive(Debug, Clone)]
/// One quad pattern: a triple pattern that may name its graph.
pub struct QuadPattern {
    /// The subject: variable, term, or quoted triple pattern.
    pub s: VarOrTerm,
    /// The predicate: variable or term.
    pub p: VarOrTerm,
    /// The object: variable, term, or quoted triple pattern.
    pub o: VarOrTerm,
    /// The graph, when the quad names one; `None` is the default graph.
    pub g: Option<VarOrTerm>,
}

#[allow(dead_code)] // SERVICE endpoint is parsed but federation is unsupported
#[derive(Debug, Clone)]
/// A solution-producing part of a query, in algebra form.
pub enum Pattern {
    /// A basic graph pattern: triple patterns joined together.
    Bgp(Vec<TriplePattern>),
    /// The solutions of both sides, joined on their shared variables.
    Join(Box<Pattern>, Box<Pattern>),
    /// A left outer join: left solutions without a match survive, with the
    /// condition evaluated on the merged solution when given.
    LeftJoin(Box<Pattern>, Box<Pattern>, Option<Expr>),
    /// The solutions of either side.
    Union(Box<Pattern>, Box<Pattern>),
    /// Left solutions whose variables all occur with the same values on the
    /// right side are removed.
    Minus(Box<Pattern>, Box<Pattern>),
    /// The inner pattern's solutions that pass the expression.
    Filter(Expr, Box<Pattern>),
    /// The inner pattern evaluated with the given graph active.
    Graph(VarOrTerm, Box<Pattern>),
    /// A variable computed for each solution of the inner pattern.
    Extend(Box<Pattern>, Var, Expr),
    /// An inline data block: one row of term-or-unbound per variable.
    Values(Vec<Var>, Vec<Vec<Option<Term>>>),
    /// A sub-select, whose projected variables are visible outside.
    SubSelect(Box<Query>),
    /// Endpoint, pattern, SILENT, and the query text sent to the endpoint.
    Service(VarOrTerm, Box<Pattern>, bool, String),
}

impl Pattern {
    /// A basic graph pattern with no triple patterns: the single empty solution.
    pub fn empty() -> Pattern {
        Pattern::Bgp(Vec::new())
    }
}

#[derive(Debug, Clone)]
/// An expression over the variables of a solution.
pub enum Expr {
    /// The value of a variable slot.
    Var(Var),
    /// A constant term.
    Const(Term),
    /// Logical disjunction.
    Or(Box<Expr>, Box<Expr>),
    /// Logical conjunction.
    And(Box<Expr>, Box<Expr>),
    /// Logical negation.
    Not(Box<Expr>),
    /// A comparison with the given operator.
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    /// Membership in a list of expressions; the flag is set for `NOT IN`.
    In(Box<Expr>, Vec<Expr>, bool), // negated
    /// `+`/`-`/`*`/`/` between two expressions.
    Arith(char, Box<Expr>, Box<Expr>),
    /// Arithmetic negation.
    Neg(Box<Expr>),
    /// `BOUND(?v)`.
    Bound(Var),
    /// `EXISTS`/`NOT EXISTS` over a pattern; the flag is set for the negated form.
    Exists(Box<Pattern>, bool), // negated
    /// Built-in (upper-case name) or IRI function call.
    Call(String, Vec<Expr>),
}

impl Expr {
    /// Call `f` on each direct sub-expression.
    pub fn for_each_child(&self, f: &mut dyn FnMut(&Expr)) {
        match self {
            Expr::Var(_) | Expr::Const(_) | Expr::Bound(_) | Expr::Exists(..) => {}
            Expr::Or(a, b) | Expr::And(a, b) | Expr::Cmp(_, a, b) | Expr::Arith(_, a, b) => {
                f(a);
                f(b);
            }
            Expr::Not(a) | Expr::Neg(a) => f(a),
            Expr::In(a, list, _) => {
                f(a);
                list.iter().for_each(|x| f(x));
            }
            Expr::Call(_, args) => args.iter().for_each(|x| f(x)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
/// The operators of a comparison.
pub enum CmpOp {
    /// `=`: term equality.
    Eq,
    /// `!=`.
    Ne,
    /// `<` on the operands' value space.
    Lt,
    /// `>`.
    Gt,
    /// `<=`.
    Le,
    /// `>=`.
    Ge,
}

#[derive(Debug, Clone)]
/// One aggregate in a query's projection, ordering or HAVING.
pub struct Aggregate {
    /// COUNT SUM MIN MAX AVG SAMPLE GROUP_CONCAT
    pub func: String,
    /// Whether `DISTINCT` values feed the aggregate.
    pub distinct: bool,
    /// The aggregated expression; `None` is `COUNT(*)`.
    pub expr: Option<Expr>,
    /// The GROUP_CONCAT separator (default space).
    pub separator: String,
}

#[derive(Debug, Clone)]
/// What a query returns.
pub enum Form {
    /// A projection of expressions to variables, with DISTINCT/REDUCED flags.
/// A projection; `None` projects every visible variable.
    Select {
        /// DISTINCT: duplicate solutions are removed.
        distinct: bool,
        /// REDUCED (parsed; treated like DISTINCT where it is allowed to be).
        reduced: bool,
        /// The projected expressions; `None` projects every visible variable.
        projection: Option<Vec<(Option<Expr>, Var)>>,
    },
    /// A template instantiated with every solution.
    Construct(Vec<TriplePattern>),
    /// A single boolean.
    Ask,
    /// The statements about each listed resource (variables included).
    Describe(Vec<VarOrTerm>),
}

#[derive(Debug, Clone, Default)]
/// The dataset a query or update declares for itself.
pub struct Dataset {
    /// IRIs of the default graph (`FROM`).
    pub default: Vec<String>,
    /// IRIs of the named graphs (`FROM NAMED`).
    pub named: Vec<String>,
}

#[derive(Debug, Clone)]
/// A parsed query.
pub struct Query {
    /// SELECT, CONSTRUCT, ASK or DESCRIBE, with their parts.
    pub form: Form,
    /// The dataset the query runs on, when it declares one.
    pub dataset: Option<Dataset>,
    /// The pattern producing the solutions.
    pub pattern: Pattern,
    /// GROUP BY keys, each optionally named.
    pub group_by: Vec<(Expr, Option<Var>)>,
    /// Conditions on the aggregated groups.
    pub having: Vec<Expr>,
    /// The aggregates used in projection, ordering or HAVING.
    pub aggregates: Vec<Aggregate>,
    /// Hidden variable slots holding aggregate results.
    pub agg_vars: Vec<Var>,
    /// Sort keys, most significant first; the flag is set for descending.
    pub order_by: Vec<(Expr, bool)>, // descending
    /// At most this many solutions survive (`LIMIT`).
    pub limit: Option<usize>,
    /// This many solutions are skipped first (`OFFSET`).
    pub offset: usize,
    /// An inline VALUES block joined with the pattern.
    pub values: Option<Pattern>,
}

#[derive(Debug, Clone)]
/// Which graphs a graph-management update works on.
pub enum GraphTarget {
    /// The default graph.
    Default,
    /// The named graphs.
    Named,
    /// The default graph and the named graphs.
    All,
    /// One named graph.
    Graph(String),
}

/// SILENT flags on graph management are parsed for fidelity but not acted
/// on: those operations never fail on missing graphs here.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum UpdateOp {
    /// Triples added to the store as given.
    InsertData(Vec<QuadPattern>),
    /// Triples removed from the store as given.
    DeleteData(Vec<QuadPattern>),
    /// The matching quads of the pattern are removed.
    DeleteWhere(Vec<QuadPattern>),
    /// DELETE/INSERT WHERE: the pattern's solutions drive the templates.
    Modify {
        /// The graph INSERT and DELETE work in when they say `WITH`.
        with: Option<String>,
        /// Quads removed for every solution.
        delete: Vec<QuadPattern>,
        /// Quads added for every solution.
        insert: Vec<QuadPattern>,
        /// The dataset the pattern sees (`USING`).
        using: Option<Dataset>,
        /// The pattern itself.
        pattern: Pattern,
    },
    /// Empty the target; the flag is its SILENT.
    Clear(GraphTarget, bool),
    /// Remove the target and its contents.
    Drop(GraphTarget, bool),
    /// Create an empty named graph.
    Create(String, bool),
    /// Load a document over HTTP(S) into a graph.
    Load {
        /// The document's URL.
        source: String,
        /// The target graph; `None` is the default graph.
        into: Option<String>,
        /// SILENT: a load that fails is not an error.
        silent: bool,
    },
    /// ADD / MOVE / COPY: (op, from, to, silent); None = default graph
    Transfer(String, Option<String>, Option<String>, bool),
}

/// Variable names by slot.
#[derive(Debug, Clone, Default)]
/// Variable names by slot.
pub struct VarTable {
    /// Slot `i` has this name.
    pub names: Vec<String>,
}

impl VarTable {
    /// The slot of `name`, adding one at the end when it is new.
    pub fn slot(&mut self, name: &str) -> Var {
        if let Some(i) = self.names.iter().position(|n| n == name) {
            return i;
        }
        self.names.push(name.to_string());
        self.names.len() - 1
    }
    /// A fresh slot invisible to the query's results (its name starts with a space).
    pub fn hidden(&mut self) -> Var {
        let n = format!(" h{}", self.names.len());
        self.slot(&n)
    }
    /// Whether the slot was made by [`Self::hidden`], so it is projected away.
    pub fn is_hidden(&self, v: Var) -> bool {
        self.names[v].starts_with(' ')
    }
    /// The number of slots.
    pub fn len(&self) -> usize {
        self.names.len()
    }
}

/// A parsed query with its variable table and effective base IRI.
pub struct ParsedQuery {
    /// The query algebra.
    pub query: Query,
    /// Names of every variable slot.
    pub vars: VarTable,
    /// The base IRI after the query's own BASE declarations.
    pub base: String,
}

/// A parsed update: a sequence of operations over a shared variable table.
pub struct ParsedUpdate {
    /// The operations, applied in order and all-or-nothing.
    pub ops: Vec<UpdateOp>,
    /// Names of every variable slot.
    pub vars: VarTable,
    /// The effective base IRI.
    pub base: String,
}
