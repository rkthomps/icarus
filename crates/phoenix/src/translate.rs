use cachet_lang::parser::Item;
use clang::{Entity, EntityKind};
use std::fmt;

/// A "small C++": the subset of the clang AST a CacheIR stub generator body
/// actually uses, with the implicit-conversion scaffolding already stripped.
/// Everything here is reachable from inside a body, so there are no
/// declaration nodes yet.
#[derive(Clone, Debug)]
pub struct CompoundStmt {
    pub stmts: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    If(IfStmt),
    Let(LetStmt),
    Return(ReturnStmt),
    /// `MOZ_ASSERT(cond)`, recovered from its `do { ... } while (0)` expansion.
    /// Kept rather than dropped as macro noise: this is what becomes `assume`.
    Assert(AssertStmt),
    /// An expression in statement position. libclang emits no wrapper node for
    /// these; the `CallExpr` hangs directly off the enclosing `CompoundStmt`.
    Expr(Expr),
}

#[derive(Clone, Debug)]
pub struct IfStmt {
    pub cond: Expr,
    pub then: CompoundStmt,
    pub els: Option<CompoundStmt>,
}

/// A `DeclStmt` wrapping a single `VarDecl`: `NumberOperandId lhs = ...`.
#[derive(Clone, Debug)]
pub struct LetStmt {
    pub name: String,
    pub ty: String,
    pub init: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct ReturnStmt {
    pub value: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct AssertStmt {
    /// `MOZ_ASSERT_IF`'s first argument: the assertion holds only where this
    /// does. `None` for a plain `MOZ_ASSERT`.
    pub guard: Option<Expr>,
    pub cond: Expr,
}

#[derive(Clone, Debug)]
pub enum Expr {
    Call(Call),
    Unary(UnaryOp),
    Binary(BinaryOp),
    Ref(Ref),
    EnumConst(EnumConst),
    Lit(Lit),
}

#[derive(Clone, Debug)]
pub struct Call {
    pub callee: Callee,
    pub args: Vec<Expr>,
}

#[derive(Clone, Debug)]
pub enum Callee {
    /// `writer.foo(..)` — the calls that become `emit` in Cachet.
    Writer(String),
    /// A free function, e.g. `EmitGuardToDoubleForToNumber`.
    Free(String),
    /// Any other method on the generator itself, e.g. `trackAttached`.
    Method(String),
}

#[derive(Clone, Debug)]
pub struct UnaryOp {
    pub op: String,
    pub operand: Box<Expr>,
}

#[derive(Clone, Debug)]
pub struct BinaryOp {
    pub op: String,
    pub lhs: Box<Expr>,
    pub rhs: Box<Expr>,
}

/// A name in expression position, resolved to what it refers to.
#[derive(Clone, Debug)]
pub enum Ref {
    /// A field on the generator: `op_`, `lhsVal_`, `rhsVal_`, `writer`.
    Field(String),
    /// A parameter of the generator method: `lhsId`, `rhsId`.
    Param(String),
    /// A local introduced by a `LetStmt`: `lhs`, `rhs`.
    Local(String),
}

/// `JSOp::StrictEq`, `AttachDecision::Attach`.
#[derive(Clone, Debug)]
pub struct EnumConst {
    pub ty: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub enum Lit {
    Str(String),
    Int(i64),
    Bool(bool),
}

/// Where an unsupported construct was found, for error reporting.
#[derive(Clone, Debug)]
pub struct Loc {
    pub file: String,
    pub line: u32,
}

impl fmt::Display for Loc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// A C++ construct outside the subset we model. Every variant names both what
/// was found and where, so an unsupported generator reports the line that
/// defeated it rather than failing anonymously.
#[derive(Clone, Debug)]
pub enum Unsupported {
    Stmt {
        kind: EntityKind,
        loc: Loc,
    },
    Expr {
        kind: EntityKind,
        loc: Loc,
    },
    /// A macro expansion with no policy entry. Its expansion is in the AST but
    /// its meaning isn't, so translating it would be guesswork.
    Macro {
        name: String,
        loc: Loc,
    },
    /// A call whose callee is neither a plain function nor a method.
    Callee {
        name: String,
        loc: Loc,
    },
    /// A node of a modeled kind whose children aren't shaped as expected.
    Malformed {
        what: String,
        loc: Loc,
    },
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Unsupported::Stmt { kind, loc } => write!(f, "{loc}: unsupported statement {kind:?}"),
            Unsupported::Expr { kind, loc } => write!(f, "{loc}: unsupported expression {kind:?}"),
            Unsupported::Macro { name, loc } => write!(f, "{loc}: unsupported macro `{name}`"),
            Unsupported::Callee { name, loc } => write!(f, "{loc}: unsupported callee `{name}`"),
            Unsupported::Malformed { what, loc } => write!(f, "{loc}: {what}"),
        }
    }
}

impl std::error::Error for Unsupported {}

pub type Result<T> = std::result::Result<T, Unsupported>;

/// Why a generator couldn't be translated. Separates "this isn't a stub
/// generator" from "this generator uses C++ we don't model": the first means
/// the caller pointed at the wrong entity, the second is a gap in the subset
/// and names the generator so a sweep over many of them stays readable.
#[derive(Clone, Debug)]
pub enum GeneratorError {
    NotAGenerator {
        what: String,
        loc: Loc,
    },
    Body {
        class: String,
        method: String,
        cause: Unsupported,
    },
}

impl fmt::Display for GeneratorError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            GeneratorError::NotAGenerator { what, loc } => write!(f, "{loc}: {what}"),
            GeneratorError::Body {
                class,
                method,
                cause,
            } => write!(f, "{class}::{method}: {cause}"),
        }
    }
}

impl std::error::Error for GeneratorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GeneratorError::NotAGenerator { .. } => None,
            GeneratorError::Body { cause, .. } => Some(cause),
        }
    }
}

fn loc(e: Entity) -> Loc {
    let Some(l) = e.get_location() else {
        return Loc {
            file: String::from("<unknown>"),
            line: 0,
        };
    };
    let l = l.get_file_location();
    Loc {
        file: l
            .file
            .map(|f| f.get_path())
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default(),
        line: l.line,
    }
}

/// A node that came from a macro has no tokens of its own: its source range
/// lies inside the expansion, so `tokenize` yields nothing.
fn is_macro_expansion(e: Entity) -> bool {
    e.get_range()
        .map(|r| r.tokenize().is_empty())
        .unwrap_or(false)
}

/// Read source text at a location, `len` characters wide.
fn text_at(l: clang::source::Location, len: usize) -> Option<String> {
    let text = l.file?.get_contents()?;
    let line = text.lines().nth(l.line.checked_sub(1)? as usize)?;
    Some(
        line.chars()
            .skip(l.column.checked_sub(1)? as usize)
            .take(len)
            .collect(),
    )
}

/// The macro that produced `e`, read from the invocation site. A cursor's
/// file location is its *expansion* location, i.e. where the macro was written.
fn macro_name(e: Entity) -> Option<String> {
    let name: String = text_at(e.get_location()?.get_file_location(), 64)?
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// libclang exposes no spelling for `UnaryOperator`, unlike `BinaryOperator`,
/// so it has to be read from source. Inside a macro expansion there are no
/// tokens, but the *spelling* location points at the operator as physically
/// written in the macro definition.
fn operator_spelling(e: Entity) -> Option<String> {
    if let Some(tok) = e
        .get_range()
        .map(|r| r.tokenize())
        .and_then(|t| t.first().map(|t| t.get_spelling()))
    {
        return Some(tok);
    }
    text_at(e.get_location()?.get_spelling_location(), 1)
}

/// Peel off the implicit nodes clang inserts: casts (`UnexposedExpr`),
/// parentheses, implicit copy constructors and conversion operators. Reading
/// `lhsVal_` costs four such wrappers before reaching the `FieldDecl`.
fn strip(mut e: Entity) -> Entity {
    loop {
        let implicit = matches!(
            e.get_reference().map(|r| r.get_kind()),
            Some(EntityKind::Constructor | EntityKind::ConversionFunction)
        );
        let peel = match e.get_kind() {
            EntityKind::UnexposedExpr | EntityKind::ParenExpr => true,
            EntityKind::CallExpr | EntityKind::MemberRefExpr if implicit => true,
            _ => false,
        };
        match e.get_children().as_slice() {
            [only] if peel => e = *only,
            _ => return e,
        }
    }
}

fn is_type_ref(e: Entity) -> bool {
    matches!(
        e.get_kind(),
        EntityKind::TypeRef | EntityKind::NamespaceRef | EntityKind::TemplateRef
    )
}

/// Translate a generator body into the modeled subset. The kind is checked
/// here so this is safe to call on any entity.
pub fn translate_compound_stmt(body: Entity) -> Result<CompoundStmt> {
    if body.get_kind() != EntityKind::CompoundStmt {
        return Err(Unsupported::Malformed {
            what: format!("expected a CompoundStmt, found {:?}", body.get_kind()),
            loc: loc(body),
        });
    }
    translate_block(body)
}

fn translate_block(block: Entity) -> Result<CompoundStmt> {
    let mut stmts = Vec::new();
    for child in block.get_children() {
        stmts.push(translate_stmt(child)?);
    }
    Ok(CompoundStmt { stmts })
}

fn translate_stmt(e: Entity) -> Result<Stmt> {
    if is_macro_expansion(e) {
        return translate_macro(e);
    }

    match e.get_kind() {
        EntityKind::IfStmt => Ok(Stmt::If(translate_if(e)?)),
        EntityKind::DeclStmt => Ok(Stmt::Let(translate_decl(e)?)),
        EntityKind::ReturnStmt => {
            let value = match e.get_children().as_slice() {
                [] => None,
                [v] => Some(translate_expr(*v)?),
                _ => {
                    return Err(Unsupported::Malformed {
                        what: String::from("return with multiple children"),
                        loc: loc(e),
                    });
                }
            };
            Ok(Stmt::Return(ReturnStmt { value }))
        }
        // Loops, switches and jumps are outside the subset. Naming them as
        // statements is clearer than letting them fall to the expression path.
        EntityKind::CompoundStmt
        | EntityKind::ForStmt
        | EntityKind::WhileStmt
        | EntityKind::DoStmt
        | EntityKind::SwitchStmt
        | EntityKind::BreakStmt
        | EntityKind::ContinueStmt
        | EntityKind::GotoStmt => Err(Unsupported::Stmt {
            kind: e.get_kind(),
            loc: loc(e),
        }),
        // Anything else in statement position is an expression statement.
        _ => Ok(Stmt::Expr(translate_expr(e)?)),
    }
}

/// Macro expansions are recognized by name and translated per policy. Anything
/// unlisted is an error: its expansion is present but its intent isn't, and
/// guessing from the expansion's shape is how `TRY_ATTACH` would get silently
/// mistaken for an assertion.
fn translate_macro(e: Entity) -> Result<Stmt> {
    let name = macro_name(e).unwrap_or_else(|| String::from("<unknown>"));
    match name.as_str() {
        "MOZ_ASSERT" | "MOZ_RELEASE_ASSERT" | "MOZ_DIAGNOSTIC_ASSERT" => {
            Ok(Stmt::Assert(AssertStmt {
                guard: None,
                cond: assert_cond(e)?,
            }))
        }
        // `do { if (cond) { MOZ_ASSERT(expr); } } while (false)`
        "MOZ_ASSERT_IF" | "MOZ_DIAGNOSTIC_ASSERT_IF" => {
            let if_stmt = do_body(e)?
                .into_iter()
                .find(|c| c.get_kind() == EntityKind::IfStmt)
                .ok_or_else(|| Unsupported::Malformed {
                    what: format!("{name} without an inner if"),
                    loc: loc(e),
                })?;
            let kids = if_stmt.get_children();
            let [cond, then, ..] = kids.as_slice() else {
                return Err(Unsupported::Malformed {
                    what: format!("{name}'s if has {} children", kids.len()),
                    loc: loc(e),
                });
            };
            let inner = then
                .get_children()
                .into_iter()
                .find(|c| c.get_kind() == EntityKind::DoStmt)
                .ok_or_else(|| Unsupported::Malformed {
                    what: format!("{name} without an inner assertion"),
                    loc: loc(e),
                })?;
            Ok(Stmt::Assert(AssertStmt {
                guard: Some(translate_expr(*cond)?),
                cond: assert_cond(inner)?,
            }))
        }
        _ => Err(Unsupported::Macro { name, loc: loc(e) }),
    }
}

/// The statements inside a macro's `do { ... } while (false)`.
fn do_body(e: Entity) -> Result<Vec<Entity>> {
    e.get_children()
        .into_iter()
        .find(|c| c.get_kind() == EntityKind::CompoundStmt)
        .map(|c| c.get_children())
        .ok_or_else(|| Unsupported::Malformed {
            what: String::from("macro expansion is not a do-block"),
            loc: loc(e),
        })
}

/// Recover `expr` from a `MOZ_ASSERT(expr)` expansion. The assertion's
/// condition is `MOZ_UNLIKELY(!MOZ_CHECK_ASSERT_ASSIGNMENT(expr))`
/// (`Assertions.h:535`), so the asserted expression sits under a chain of
/// macro-written negations and parens.
fn assert_cond(do_stmt: Entity) -> Result<Expr> {
    let if_stmt = do_body(do_stmt)?
        .into_iter()
        .find(|c| c.get_kind() == EntityKind::IfStmt)
        .ok_or_else(|| Unsupported::Malformed {
            what: String::from("assertion without a check"),
            loc: loc(do_stmt),
        })?;
    let cond =
        if_stmt
            .get_children()
            .into_iter()
            .next()
            .ok_or_else(|| Unsupported::Malformed {
                what: String::from("assertion check without a condition"),
                loc: loc(do_stmt),
            })?;
    translate_expr(peel_assert_glue(cond, &loc(do_stmt).file))
}

/// Descend through the assertion macro's wrappers to the asserted expression.
///
/// Counting negations doesn't work: the glue contributes an odd number of `!`,
/// but `MOZ_ASSERT_IF(.., !IsEqualityOp(op_))` starts with a `!` of its own,
/// and peeling that one inverts the assertion. The discriminator is *where the
/// token was written*: glue is spelled inside `Assertions.h` / `Likely.h`,
/// while the asserted expression is spelled at the call site.
fn peel_assert_glue<'tu>(mut e: Entity<'tu>, user_file: &str) -> Entity<'tu> {
    loop {
        if spelling_file(e).as_deref() == Some(user_file) {
            return e;
        }
        let kids = e.get_children();
        match e.get_kind() {
            EntityKind::UnexposedExpr | EntityKind::ParenExpr | EntityKind::UnaryOperator => {
                match kids.as_slice() {
                    [only] => e = *only,
                    _ => return e,
                }
            }
            // MOZ_UNLIKELY(x) is __builtin_expect(!!(x), 0); the first child is
            // the callee, the second the condition.
            EntityKind::CallExpr
                if e.get_name().as_deref() == Some("__builtin_expect") && kids.len() >= 2 =>
            {
                e = kids[1];
            }
            _ => return e,
        }
    }
}

/// The file a node's tokens are physically written in, which for a macro
/// expansion is the macro's own definition rather than the call site.
fn spelling_file(e: Entity) -> Option<String> {
    let l = e.get_location()?.get_spelling_location();
    l.file?
        .get_path()
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
}

fn translate_if(e: Entity) -> Result<IfStmt> {
    let kids = e.get_children();
    let (cond, then, els) = match kids.as_slice() {
        [cond, then] => (*cond, *then, None),
        [cond, then, els] => (*cond, *then, Some(*els)),
        _ => {
            return Err(Unsupported::Malformed {
                what: format!("if with {} children", kids.len()),
                loc: loc(e),
            });
        }
    };
    Ok(IfStmt {
        cond: translate_expr(cond)?,
        then: translate_branch(then)?,
        els: els.map(translate_branch).transpose()?,
    })
}

/// A branch is a block, or a single statement we wrap into one.
fn translate_branch(e: Entity) -> Result<CompoundStmt> {
    if e.get_kind() == EntityKind::CompoundStmt {
        return translate_block(e);
    }
    Ok(CompoundStmt {
        stmts: vec![translate_stmt(e)?],
    })
}

fn translate_decl(e: Entity) -> Result<LetStmt> {
    let var = match e.get_children().as_slice() {
        [var] if var.get_kind() == EntityKind::VarDecl => *var,
        kids => {
            return Err(Unsupported::Malformed {
                what: format!("declaration of {} entities", kids.len()),
                loc: loc(e),
            });
        }
    };

    let name = var.get_name().ok_or_else(|| Unsupported::Malformed {
        what: String::from("unnamed variable"),
        loc: loc(var),
    })?;
    let ty = var
        .get_type()
        .ok_or_else(|| Unsupported::Malformed {
            what: format!("variable {name} has no type"),
            loc: loc(var),
        })?
        .get_canonical_type()
        .get_display_name();
    let init = var
        .get_children()
        .into_iter()
        .find(|c| !is_type_ref(*c))
        .map(translate_expr)
        .transpose()?;

    Ok(LetStmt { name, ty, init })
}

fn translate_expr(e: Entity) -> Result<Expr> {
    let e = strip(e);
    match e.get_kind() {
        EntityKind::CallExpr => Ok(Expr::Call(translate_call(e)?)),
        EntityKind::DeclRefExpr | EntityKind::MemberRefExpr => translate_ref(e),
        EntityKind::UnaryOperator => {
            let operand =
                e.get_children()
                    .into_iter()
                    .next()
                    .ok_or_else(|| Unsupported::Malformed {
                        what: String::from("unary operator without operand"),
                        loc: loc(e),
                    })?;
            let op = operator_spelling(e).ok_or_else(|| Unsupported::Malformed {
                what: String::from("unary operator with no readable spelling"),
                loc: loc(e),
            })?;
            Ok(Expr::Unary(UnaryOp {
                op,
                operand: Box::new(translate_expr(operand)?),
            }))
        }
        EntityKind::BinaryOperator => {
            let kids = e.get_children();
            let [lhs, rhs] = kids.as_slice() else {
                return Err(Unsupported::Malformed {
                    what: format!("binary operator with {} operands", kids.len()),
                    loc: loc(e),
                });
            };
            let op = e.get_name().ok_or_else(|| Unsupported::Malformed {
                what: String::from("binary operator with no spelling"),
                loc: loc(e),
            })?;
            Ok(Expr::Binary(BinaryOp {
                op,
                lhs: Box::new(translate_expr(*lhs)?),
                rhs: Box::new(translate_expr(*rhs)?),
            }))
        }
        EntityKind::IntegerLiteral | EntityKind::StringLiteral | EntityKind::BoolLiteralExpr => {
            translate_lit(e)
        }
        kind => Err(Unsupported::Expr { kind, loc: loc(e) }),
    }
}

fn translate_call(e: Entity) -> Result<Call> {
    let name = e.get_name().unwrap_or_default();
    let target = e.get_reference().ok_or_else(|| Unsupported::Callee {
        name: name.clone(),
        loc: loc(e),
    })?;

    let kids = e.get_children();
    let (callee_expr, args) = kids.split_first().ok_or_else(|| Unsupported::Malformed {
        what: format!("call to {name} with no callee"),
        loc: loc(e),
    })?;

    let callee = match target.get_kind() {
        EntityKind::FunctionDecl | EntityKind::FunctionTemplate => Callee::Free(name),
        EntityKind::Method => {
            // `writer.foo(..)`: the callee's own child is the object, and when
            // that resolves to the generator's `writer` field the call is an
            // emit rather than an ordinary method call.
            let on_writer = callee_expr
                .get_children()
                .into_iter()
                .map(strip)
                .any(|obj| {
                    obj.get_reference().map(|r| r.get_kind()) == Some(EntityKind::FieldDecl)
                        && obj.get_name().as_deref() == Some("writer")
                });
            if on_writer {
                Callee::Writer(name)
            } else {
                Callee::Method(name)
            }
        }
        _ => return Err(Unsupported::Callee { name, loc: loc(e) }),
    };

    let args = args
        .iter()
        .map(|a| translate_expr(*a))
        .collect::<Result<Vec<_>>>()?;
    Ok(Call { callee, args })
}

fn translate_ref(e: Entity) -> Result<Expr> {
    let name = e.get_name().unwrap_or_default();
    let target = e.get_reference().ok_or_else(|| Unsupported::Expr {
        kind: e.get_kind(),
        loc: loc(e),
    })?;
    match target.get_kind() {
        EntityKind::EnumConstantDecl => {
            let ty = target
                .get_semantic_parent()
                .and_then(|p| p.get_name())
                .ok_or_else(|| Unsupported::Malformed {
                    what: format!("enum constant {name} has no enum"),
                    loc: loc(e),
                })?;
            Ok(Expr::EnumConst(EnumConst { ty, name }))
        }
        EntityKind::ParmDecl => Ok(Expr::Ref(Ref::Param(name))),
        EntityKind::VarDecl => Ok(Expr::Ref(Ref::Local(name))),
        EntityKind::FieldDecl => Ok(Expr::Ref(Ref::Field(name))),
        _ => Err(Unsupported::Expr {
            kind: e.get_kind(),
            loc: loc(e),
        }),
    }
}

/// Literals prefer `clang_Cursor_Evaluate`, which yields typed values and
/// works inside macro expansions where there are no tokens. It doesn't cover
/// every literal kind, so plain token text is the fallback.
fn translate_lit(e: Entity) -> Result<Expr> {
    use clang::EvaluationResult::*;
    let malformed = || Unsupported::Malformed {
        what: format!("unreadable {:?}", e.get_kind()),
        loc: loc(e),
    };
    let is_bool = e.get_kind() == EntityKind::BoolLiteralExpr;

    match e.evaluate() {
        Some(SignedInteger(n)) if is_bool => return Ok(Expr::Lit(Lit::Bool(n != 0))),
        Some(SignedInteger(n)) => return Ok(Expr::Lit(Lit::Int(n))),
        Some(UnsignedInteger(n)) if is_bool => return Ok(Expr::Lit(Lit::Bool(n != 0))),
        Some(UnsignedInteger(n)) => {
            return Ok(Expr::Lit(Lit::Int(
                i64::try_from(n).map_err(|_| malformed())?,
            )));
        }
        Some(String(s)) | Some(CFString(s)) | Some(ObjCString(s)) | Some(Other(s)) => {
            return Ok(Expr::Lit(Lit::Str(s.to_string_lossy().into_owned())));
        }
        _ => {}
    }

    let text = e
        .get_range()
        .map(|r| r.tokenize())
        .unwrap_or_default()
        .iter()
        .map(|t| t.get_spelling())
        .collect::<Vec<_>>()
        .join("");
    match e.get_kind() {
        EntityKind::StringLiteral => Ok(Expr::Lit(Lit::Str(text.trim_matches('"').to_string()))),
        EntityKind::BoolLiteralExpr => match text.as_str() {
            "true" => Ok(Expr::Lit(Lit::Bool(true))),
            "false" => Ok(Expr::Lit(Lit::Bool(false))),
            _ => Err(malformed()),
        },
        EntityKind::IntegerLiteral => text
            .trim_end_matches(['u', 'U', 'l', 'L'])
            .parse()
            .map(|n| Expr::Lit(Lit::Int(n)))
            .map_err(|_| malformed()),
        kind => Err(Unsupported::Expr { kind, loc: loc(e) }),
    }
}

pub struct GeneratorParam {
    pub name: String,
    pub ty: String,
}

pub struct GeneratorImpl {
    pub class: String,
    pub method: String,
    pub params: Vec<GeneratorParam>,
    pub body: CompoundStmt,
}

/// Finds a generator's shape and translates its body into the modeled subset.
/// Everything downstream works on the result, never on clang entities.
pub fn get_generator_impl(
    generator: &Entity<'_>,
) -> std::result::Result<GeneratorImpl, GeneratorError> {
    let generator = *generator;

    if generator.get_kind() != EntityKind::Method {
        return Err(GeneratorError::NotAGenerator {
            what: format!("expected a method, found {:?}", generator.get_kind()),
            loc: loc(generator),
        });
    }
    // A declaration has no body to translate; we need the out-of-line
    // definition in CacheIR.cpp, not the one in CacheIRGenerator.h.
    if !generator.is_definition() {
        return Err(GeneratorError::NotAGenerator {
            what: String::from("expected a method definition, found a declaration"),
            loc: loc(generator),
        });
    }

    // Semantic, not lexical: the body lives in CacheIR.cpp while the class is
    // declared in CacheIRGenerator.h, so the lexical parent is the namespace.
    let class = generator
        .get_semantic_parent()
        .filter(|p| matches!(p.get_kind(), EntityKind::ClassDecl | EntityKind::StructDecl))
        .and_then(|p| p.get_name())
        .ok_or_else(|| GeneratorError::NotAGenerator {
            what: String::from("method has no owning class"),
            loc: loc(generator),
        })?;

    let method = generator
        .get_name()
        .ok_or_else(|| GeneratorError::NotAGenerator {
            what: String::from("method has no name"),
            loc: loc(generator),
        })?;

    let params = generator
        .get_arguments()
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let name = p.get_name().ok_or_else(|| GeneratorError::NotAGenerator {
                what: format!("unnamed parameter in {class}::{method}"),
                loc: loc(p),
            })?;
            // Canonical: typedefs and aliases stripped, so the translator keys
            // on `js::jit::ValOperandId` rather than however it was spelled.
            let ty = p
                .get_type()
                .ok_or_else(|| GeneratorError::NotAGenerator {
                    what: format!("parameter {name} has no type"),
                    loc: loc(p),
                })?
                .get_canonical_type()
                .get_display_name();
            Ok(GeneratorParam { name, ty })
        })
        .collect::<std::result::Result<Vec<_>, GeneratorError>>()?;

    let body = generator
        .get_children()
        .into_iter()
        .find(|c| c.get_kind() == EntityKind::CompoundStmt)
        .ok_or_else(|| GeneratorError::NotAGenerator {
            what: format!("{class}::{method} has no body"),
            loc: loc(generator),
        })?;

    let body = translate_compound_stmt(body).map_err(|cause| GeneratorError::Body {
        class: class.clone(),
        method: method.clone(),
        cause,
    })?;

    Ok(GeneratorImpl {
        class,
        method,
        params,
        body,
    })
}

// Dumping ____________________________________________________________________
//
// Mirrors the raw clang dump in `main.rs`: one node per line, two spaces per
// level, names in backticks. Reading the two side by side is the quickest way
// to see what the translation dropped.

fn indent(f: &mut fmt::Formatter, depth: usize) -> fmt::Result {
    write!(f, "{:indent$}", "", indent = depth * 2)
}

impl fmt::Display for GeneratorImpl {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "Method `{}` [{}]", self.method, self.class)?;
        for p in &self.params {
            writeln!(f, "  ParmDecl `{}` : {}", p.name, p.ty)?;
        }
        fmt_block(f, &self.body, 1)
    }
}

impl fmt::Display for CompoundStmt {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt_block(f, self, 0)
    }
}

fn fmt_block(f: &mut fmt::Formatter, block: &CompoundStmt, depth: usize) -> fmt::Result {
    indent(f, depth)?;
    writeln!(f, "CompoundStmt")?;
    for stmt in &block.stmts {
        fmt_stmt(f, stmt, depth + 1)?;
    }
    Ok(())
}

/// A labelled sub-tree, for the places where position alone is ambiguous
/// (an `if`'s else arm, an assertion's guard).
fn fmt_labelled(f: &mut fmt::Formatter, label: &str, expr: &Expr, depth: usize) -> fmt::Result {
    indent(f, depth)?;
    writeln!(f, "{label}")?;
    fmt_expr(f, expr, depth + 1)
}

fn fmt_stmt(f: &mut fmt::Formatter, stmt: &Stmt, depth: usize) -> fmt::Result {
    match stmt {
        Stmt::If(s) => {
            indent(f, depth)?;
            writeln!(f, "IfStmt")?;
            fmt_expr(f, &s.cond, depth + 1)?;
            fmt_block(f, &s.then, depth + 1)?;
            if let Some(els) = &s.els {
                indent(f, depth + 1)?;
                writeln!(f, "Else")?;
                fmt_block(f, els, depth + 2)?;
            }
            Ok(())
        }
        Stmt::Let(s) => {
            indent(f, depth)?;
            writeln!(f, "VarDecl `{}` : {}", s.name, s.ty)?;
            match &s.init {
                Some(init) => fmt_expr(f, init, depth + 1),
                None => Ok(()),
            }
        }
        Stmt::Return(s) => {
            indent(f, depth)?;
            writeln!(f, "ReturnStmt")?;
            match &s.value {
                Some(v) => fmt_expr(f, v, depth + 1),
                None => Ok(()),
            }
        }
        Stmt::Assert(s) => {
            indent(f, depth)?;
            writeln!(f, "AssertStmt")?;
            if let Some(guard) = &s.guard {
                fmt_labelled(f, "Guard", guard, depth + 1)?;
            }
            fmt_labelled(f, "Cond", &s.cond, depth + 1)
        }
        Stmt::Expr(e) => fmt_expr(f, e, depth),
    }
}

fn fmt_expr(f: &mut fmt::Formatter, expr: &Expr, depth: usize) -> fmt::Result {
    indent(f, depth)?;
    match expr {
        Expr::Call(c) => {
            let (kind, name) = match &c.callee {
                Callee::Writer(n) => ("Writer", n),
                Callee::Free(n) => ("Free", n),
                Callee::Method(n) => ("Method", n),
            };
            writeln!(f, "Call {kind} `{name}`")?;
            for arg in &c.args {
                fmt_expr(f, arg, depth + 1)?;
            }
            Ok(())
        }
        Expr::Unary(u) => {
            writeln!(f, "Unary `{}`", u.op)?;
            fmt_expr(f, &u.operand, depth + 1)
        }
        Expr::Binary(b) => {
            writeln!(f, "Binary `{}`", b.op)?;
            fmt_expr(f, &b.lhs, depth + 1)?;
            fmt_expr(f, &b.rhs, depth + 1)
        }
        Expr::Ref(r) => match r {
            Ref::Field(n) => writeln!(f, "Field `{n}`"),
            Ref::Param(n) => writeln!(f, "Param `{n}`"),
            Ref::Local(n) => writeln!(f, "Local `{n}`"),
        },
        Expr::EnumConst(e) => writeln!(f, "EnumConst `{}::{}`", e.ty, e.name),
        Expr::Lit(l) => match l {
            Lit::Str(s) => writeln!(f, "Lit `{s:?}`"),
            Lit::Int(n) => writeln!(f, "Lit `{n}`"),
            Lit::Bool(b) => writeln!(f, "Lit `{b}`"),
        },
    }
}

fn translate(generator: Entity) -> std::result::Result<Vec<Item>, std::string::String> {
    let generator_impl = get_generator_impl(&generator);
    Err(std::string::String::from("Ni"))
}
