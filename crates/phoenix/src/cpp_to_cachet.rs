use std::collections::{HashSet, VecDeque};
use std::fmt;

use cachet_lang::ast::{
    BinOper, CompareBinOper, Ident, LogicalBinOper, Path as CachetPath, Spanned,
};
use cachet_lang::ast::{NegateKind, VarParamKind};
use cachet_lang::parser::{
    Arg, BinOperExpr, Block, Call, CallableItem, Comment, ElseClause, Expr, GlobalVarItem,
    IfStmt as CachetIfStmt, IrItem, Item, LetStmt, LocalVar, Mod, NegateExpr,
    Param as CachetParam, RetStmt, Stmt, VarParam,
};
use clang::Entity;

use crate::cpp_subset::{
    Call as CppCall, Callee as CppCallee, CompoundStmt as CppCompoundStmt, Expr as CppExpr, FnDef,
    Indirection,
    Error as SubsetError, FnId, FnRef, Param, RefKind, Span as CppSpan, Spanned as CppSpanned,
    Stmt as CppStmt, Type as CppType, get_fn_def, walk_block,
};
use crate::{
    clang_utils::{find_definition, get_errors, parse_file},
    cpp_subset::{GenDef, Ref, Visit, get_gen_def},
};

/// A C++ construct with no Cachet counterpart yet.
///
/// Translation refuses rather than guesses: a type mapped wrongly would verify
/// something other than the code that runs, which is worse than not verifying.
#[derive(Clone, Debug)]
pub struct Unhandled {
    pub what: String,
    /// `Unknown` where the construct has no span to point at -- types and
    /// parameters aren't spanned, only statements and expressions.
    pub span: CppSpan,
}

impl Unhandled {
    fn new(what: impl Into<String>) -> Self {
        Unhandled {
            what: what.into(),
            span: CppSpan::Unknown,
        }
    }

    /// Attach a location, for errors raised where one is in scope.
    fn at(mut self, span: &CppSpan) -> Self {
        self.span = span.clone();
        self
    }
}

impl fmt::Display for Unhandled {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.span {
            CppSpan::Unknown => write!(f, "unhandled {}", self.what),
            span => write!(f, "{span}: unhandled {}", self.what),
        }
    }
}

impl std::error::Error for Unhandled {}

/// C++ type to Cachet type.
///
/// Deliberately a short, explicit table. Every entry asserts that the two
/// types denote the same values, which has to be argued case by case, so
/// entries are added one at a time and anything absent is [`Unhandled`].
fn translate_type(ty: &CppType) -> Result<CachetPath, Unhandled> {
    match ty.indirection {
        Indirection::Value => {}
        // `const Value&` is pass-by-reference only to avoid a copy; it denotes
        // the value it refers to and cannot change it, so it translates as that
        // value.
        Indirection::Ref if ty.is_const => {}
        // A mutable reference or a pointer is aliased state: the callee can
        // write through it, which a Cachet value cannot express.
        Indirection::Ref | Indirection::Ptr => {
            return Err(Unhandled::new(format!(
                "type `{}`: {:?} indirection",
                ty.spelled, ty.indirection
            )));
        }
    }

    let scope: Vec<&str> = ty.scope.iter().map(String::as_str).collect();
    let args: Vec<Vec<&str>> = ty
        .args
        .iter()
        .map(|arg| arg.scope.iter().map(String::as_str).collect())
        .collect();

    match (scope.as_slice(), args.as_slice()) {
        // `HandleValue` is `JS::Handle<JS::Value>`, a rooted reference to a
        // `Value`. Rooting exists to tell the GC where live pointers are
        // (RootingAPI.h, "[SMDOC] Stack Rooting"); Cachet models no GC, so the
        // wrapper carries no meaning and a handle denotes exactly its `Value`.
        (["JS", "Handle"], [inner]) if inner.as_slice() == ["JS", "Value"] => {
            Ok(CachetPath::from_ident("Value"))
        }

        // `JS::Value` itself, however it is spelled at the use site: by value,
        // or as the `const Value&` a helper takes.
        (["JS", "Value"], []) => Ok(CachetPath::from_ident("Value")),

        // `enum class JSOp` (Opcodes.h) against `enum JSOp` (notes/jsop.cachet).
        // Same name, same role: the bytecode op a generator is attaching for.
        (["JSOp"], []) => Ok(CachetPath::from_ident("JSOp")),

        // An input operand holding a value. `CacheIR::defineInputValueId`
        // returns `ValueId` (notes/cacheir.cachet:459), which is what a
        // `ValOperandId` denotes on the Cachet side.
        //
        // The rest of the `*OperandId` family maps the same way when needed:
        // `ObjOperandId`/`ObjectId`, `NumberOperandId`/`NumberId`, and so on
        // (notes/cacheir.cachet:143-215).
        (["js", "jit", "ValOperandId"], []) => Ok(CachetPath::from_ident("ValueId")),

        // `struct NumberId <: ValueId` (notes/cacheir.cachet:215).
        (["js", "jit", "NumberOperandId"], []) => Ok(CachetPath::from_ident("NumberId")),

        // `struct Int32Id <: OperandId` (notes/cacheir.cachet:203).
        (["js", "jit", "Int32OperandId"], []) => Ok(CachetPath::from_ident("Int32Id")),

        // `bool` against Cachet's `Bool`.
        (["bool"], []) => Ok(CachetPath::from_ident("Bool")),

        _ => Err(Unhandled::new(format!(
            "type `{}` (canonically `{}`)",
            ty.spelled,
            ty.scope.join("::")
        ))),
    }
}

#[derive(Default)]
struct Fields(Vec<Ref>);

impl Visit for Fields {
    fn visit_ref(&mut self, r: &Ref) {
        if r.kind == RefKind::Field && !self.0.iter().any(|f| f.name == r.name) {
            self.0.push(r.clone());
        }
    }
}

fn get_gen_def_fields(gen_def: &GenDef) -> Vec<Ref> {
    let mut fields = Fields::default();
    walk_block(&mut fields, &gen_def.body);
    fields.0
}

/// `js::jit::CacheIRWriter`, the type of a generator's `writer` field.
const CACHE_IR_WRITER: [&str; 3] = ["js", "jit", "CacheIRWriter"];

/// The writer is how C++ emits CacheIR, which Cachet makes implicit. So a
/// writer is never a value: it is dropped as an argument and as a parameter,
/// and a call on it becomes an `emit`.
fn is_writer(ty: &CppType) -> bool {
    ty.scope == CACHE_IR_WRITER
}

/// Whether an expression is the writer itself, for dropping it as an argument.
fn is_writer_expr(expr: &CppSpanned<CppExpr>) -> bool {
    matches!(&expr.value, CppExpr::Ref(r) if is_writer(&r.ty))
}

/// The method name, if this is a call on the writer.
fn writer_call(call: &CppCall) -> Option<&str> {
    match &call.callee {
        CppCallee::Method {
            recv: Some(recv),
            callee,
        } if is_writer_expr(&**recv) => Some(&callee.name),
        _ => None,
    }
}

/// `js::jit::ValOperandId`, a generator's input operand.
const VAL_OPERAND_ID: [&str; 3] = ["js", "jit", "ValOperandId"];

/// `f()`, with no arguments.
fn invoke(target: CachetPath) -> Expr {
    Expr::Invoke(Call {
        target: Spanned::internal(target),
        args: Spanned::internal(Vec::new()),
    })
}

/// The statements every stub generator opens with:
///
/// ```text
/// initRegState();
/// let lhsId = CacheIR::defineInputValueId();
/// let rhsId = CacheIR::defineInputValueId();
/// initValueOutput();
/// ```
///
/// None of this comes from the generator's body. It is the calling convention
/// the C++ inherits from `tryAttachStub`, which sets up register state and
/// declares the input operands before dispatching to a generator. Since each
/// generator is translated in isolation, the preamble is synthesized here.
///
/// The `let`s stand in for the method's parameters -- one each, named as C++
/// names them -- so the body can refer to a parameter as an ordinary local.
fn translate_preamble(params: &[Param]) -> Result<Vec<Spanned<Stmt>>, Unhandled> {
    // Only the two-value-operand shape is understood so far. Another operand
    // kind would need its own `defineInput*`, and a non-value output would need
    // something other than `initValueOutput`.
    if let Some(other) = params.iter().find(|p| p.ty.scope != VAL_OPERAND_ID) {
        return Err(Unhandled::new(format!(
            "parameter `{}`: expected a ValOperandId, found `{}`",
            other.name, other.ty.spelled
        )));
    }
    if params.len() != 2 {
        return Err(Unhandled::new(format!(
            "generator takes {} operands, expected 2",
            params.len()
        )));
    }

    let mut stmts = vec![Spanned::internal(Stmt::Expr(invoke(
        CachetPath::from_ident("initRegState"),
    )))];

    stmts.extend(params.iter().map(|param| {
        Spanned::internal(Stmt::Let(LetStmt {
            lhs: LocalVar {
                ident: Spanned::internal(Ident::from(param.name.clone())),
                is_mut: false,
                // Inferred from the initializer.
                type_: None,
            },
            rhs: Spanned::internal(invoke(
                CachetPath::from_ident("CacheIR").nest(Ident::from("defineInputValueId")),
            )),
        }))
    }));

    stmts.push(Spanned::internal(Stmt::Expr(invoke(
        CachetPath::from_ident("initValueOutput"),
    ))));

    Ok(stmts)
}

/// `var lhsVal_: Value;` for each of the generator's fields.
///
/// Names are kept exactly as C++ spells them -- `lhsVal_`, not `lhsValue` --
/// so a generated name always traces back to its source.
fn create_field_var_items(fields: &[Ref]) -> Result<Vec<Spanned<Item>>, Unhandled> {
    fields
        .iter()
        .map(|field| {
            let type_ = translate_type(&field.ty)
                .map_err(|e| Unhandled::new(format!("field `{}`: {}", field.name, e.what)))?;
            Ok(Spanned::internal(Item::GlobalVar(GlobalVarItem {
                ident: Spanned::internal(Ident::from(field.name.clone())),
                attrs: Vec::new(),
                is_mut: false,
                type_: Spanned::internal(type_),
                value: None,
            })))
        })
        .collect()
}

/// What kind of C++ is being translated, which decides both where the item
/// sits and which entities are ambient to it.
///
/// An ambient entity is one the unit carries implicitly and translation
/// reinterprets rather than translates: a generator's `writer` is the CacheIR
/// sink, its `AttachDecision` is the dispatcher protocol, an instruction's
/// `masm` and `allocator` are the machine.
///
/// The nesting mirrors Cachet's, which is one level deep: `ParentIndex` is a
/// type or an `ir`, and callables can't nest. Carrying the parent inside the
/// kind keeps a helper from having one.
enum Unit {
    /// An `op` in the generator's own `ir`.
    StubGenerator { ir: Ident },
    /// An `op` in `ir CacheIR`, from `CacheIRCompiler::emit*`.
    #[allow(dead_code)] // until instructions are translated.
    Instruction { ir: Ident },
    /// A top-level `fn`. Nothing is ambient.
    Helper,
}

impl Unit {
    /// The path a field qualifies against, or `None` where there's no parent.
    fn parent(&self) -> Option<CachetPath> {
        match self {
            Unit::StubGenerator { ir } | Unit::Instruction { ir } => {
                Some(CachetPath::from_ident(*ir))
            }
            Unit::Helper => None,
        }
    }
}

/// Where the item being translated sits. Flows down; immutable.
struct Ctx {
    unit: Unit,
}

/// A C++ method to the Cachet function that models it.
///
/// Keyed on the receiver's *translated* type, so `lhsVal_.isNumber()` (whose
/// receiver is a `HandleValue`) and `v.isNumber()` (a `const Value&`) reach the
/// same entry: both receivers translate to `Value`.
///
/// The names do not always match, which is why this is a table and not a rule:
/// C++ spells it `isBoolean`, the model spells it `isBool`.
fn translate_method(recv_ty: CachetPath, method: &str) -> Option<CachetPath> {
    let value = CachetPath::from_ident("Value");
    let name = match (recv_ty, method) {
        // `impl Value` in notes/js.cachet.
        (ty, "isNumber") if ty == value => "isNumber",
        (ty, "isInt32") if ty == value => "isInt32",
        (ty, "isBoolean") if ty == value => "isBool",
        (ty, "isNull") if ty == value => "isNull",
        (ty, "isNullOrUndefined") if ty == value => "isNullOrUndefined",
        _ => return None,
    };
    // `impl Value { fn isNumber(value: Value) }` is called as
    // `Value::isNumber(v)`, so the C++ receiver becomes the first argument.
    Some(recv_ty.nest(Ident::from(name)))
}

/// A C++ free function to the Cachet function that models it.
///
/// A miss means the definition is translated instead. The CacheIR machinery --
/// stub generators, their helpers, and the instruction semantics in
/// `CacheIRCompiler` -- is meant to be translated, so entries here are for
/// engine functions outside it, where translation should stop.
fn translate_free(_name: &str) -> Option<CachetPath> {
    None
}

/// A C++ binary operator to Cachet's.
fn translate_bin_oper(op: &str) -> Option<BinOper> {
    Some(match op {
        "||" => BinOper::Logical(LogicalBinOper::Or),
        "&&" => BinOper::Logical(LogicalBinOper::And),
        "==" => BinOper::Compare(CompareBinOper::Eq),
        "!=" => BinOper::Compare(CompareBinOper::Neq),
        _ => return None,
    })
}

/// A C++ unary operator to Cachet's.
///
/// Cachet's unary operators are all negations, so `&` and `*` have no
/// counterpart -- taking an address or dereferencing is aliasing, which a value
/// language cannot express.
fn translate_unary_oper(op: &str) -> Option<NegateKind> {
    Some(match op {
        "!" => NegateKind::Logical,
        "-" => NegateKind::Arith,
        "~" => NegateKind::Bitwise,
        _ => return None,
    })
}

/// The declared type of whatever an expression names.
///
/// Needed to key [`translate_method`]: only a name carries a type in the
/// subset, so a receiver that is anything else cannot be looked up.
fn named_type(expr: &CppSpanned<CppExpr>) -> Result<&CppType, Unhandled> {
    match &expr.value {
        CppExpr::Ref(r) => Ok(&r.ty),
        _ => Err(Unhandled::new(String::from("receiver is not a name"))),
    }
}

/// Errors from a sub-expression already point at the narrowest construct that
/// failed, so a span is filled in only where none was set.
fn translate_expr(
    ctx: &Ctx,
    needed: &mut Vec<FnRef>,
    expr: &CppSpanned<CppExpr>,
) -> Result<Expr, Unhandled> {
    translate_expr_value(ctx, needed, expr).map_err(|e| match e.span {
        CppSpan::Unknown => e.at(&expr.span),
        _ => e,
    })
}

fn translate_expr_value(
    ctx: &Ctx,
    needed: &mut Vec<FnRef>,
    expr: &CppSpanned<CppExpr>,
) -> Result<Expr, Unhandled> {
    match &expr.value {
        CppExpr::Ref(r) => match r.kind {
            RefKind::Param | RefKind::Local => Ok(Expr::Var(Spanned::internal(
                CachetPath::from_ident(Ident::from(r.name.clone())),
            ))),
            // A field is a `var` on the enclosing item, and Cachet requires it
            // qualified: `field` alone doesn't resolve inside an `op`.
            RefKind::Field => {
                let parent = ctx.unit.parent().ok_or_else(|| {
                    Unhandled::new(format!("field `{}` outside an ir", r.name))
                })?;
                Ok(Expr::Var(Spanned::internal(
                    parent.nest(Ident::from(r.name.clone())),
                )))
            }
        },

        CppExpr::Call(call) => match &call.callee {
            CppCallee::Method {
                recv: Some(recv),
                callee,
            } => {
                let recv_ty = translate_type(named_type(recv)?)?;
                let target = translate_method(recv_ty, &callee.name)
                    .ok_or_else(|| Unhandled::new(format!("method `{}`", callee.name)))?;
                // The receiver leads, then the C++ arguments.
                let mut args = vec![Spanned::internal(Arg::Expr(translate_expr(
                    ctx, needed, recv,
                )?))];
                for arg in &call.args {
                    args.push(Spanned::internal(Arg::Expr(translate_expr(
                        ctx, needed, arg,
                    )?)));
                }
                Ok(Expr::Invoke(Call {
                    target: Spanned::internal(target),
                    args: Spanned::internal(args),
                }))
            }
            CppCallee::Method { recv: None, callee } => Err(Unhandled::new(format!(
                "method `{}` on an implicit `this`",
                callee.name
            ))),
            CppCallee::Free(callee) => {
                // Modelled, or translated: on a miss the C++ definition is
                // recorded for the caller to translate, and the call is emitted
                // against its own name.
                let target = translate_free(&callee.name).unwrap_or_else(|| {
                    needed.push(callee.clone());
                    CachetPath::from_ident(Ident::from(callee.name.clone()))
                });
                let args = call
                    .args
                    .iter()
                    // The writer is ambient in Cachet, so it isn't passed.
                    .filter(|arg| !is_writer_expr(arg))
                    .map(|arg| {
                        Ok(Spanned::internal(Arg::Expr(translate_expr(
                            ctx, needed, arg,
                        )?)))
                    })
                    .collect::<Result<Vec<_>, Unhandled>>()?;
                Ok(Expr::Invoke(Call {
                    target: Spanned::internal(target),
                    args: Spanned::internal(args),
                }))
            }
        },

        CppExpr::Unary(unary) => {
            let kind = translate_unary_oper(&unary.op)
                .ok_or_else(|| Unhandled::new(format!("unary `{}`", unary.op)))?;
            Ok(Expr::Negate(Box::new(NegateExpr {
                kind: Spanned::internal(kind),
                expr: Spanned::internal(translate_expr(ctx, needed, &unary.operand)?),
            })))
        }

        CppExpr::Binary(binary) => {
            let oper = translate_bin_oper(&binary.op)
                .ok_or_else(|| Unhandled::new(format!("binary `{}`", binary.op)))?;
            Ok(Expr::BinOper(Box::new(BinOperExpr {
                oper: Spanned::internal(oper),
                lhs: Spanned::internal(translate_expr(ctx, needed, &binary.lhs)?),
                rhs: Spanned::internal(translate_expr(ctx, needed, &binary.rhs)?),
            })))
        }

        CppExpr::Construct(c) => Err(Unhandled::new(format!("construction of `{}`", c.ty.spelled))),
        CppExpr::EnumConst(e) => Err(Unhandled::new(format!("enum constant `{}::{}`", e.ty, e.name))),
        CppExpr::Lit(_) => Err(Unhandled::new(String::from("literal"))),
        CppExpr::This => Err(Unhandled::new(String::from("`this`"))),
    }
}

/// One C++ statement can yield several, so this returns a list.
fn translate_stmt(
    ctx: &Ctx,
    needed: &mut Vec<FnRef>,
    stmt: &CppSpanned<CppStmt>,
) -> Result<Vec<Spanned<Stmt>>, Unhandled> {
    translate_stmt_values(ctx, needed, stmt).map_err(|e| match e.span {
        CppSpan::Unknown => e.at(&stmt.span),
        _ => e,
    })
}

fn translate_stmt_values(
    ctx: &Ctx,
    needed: &mut Vec<FnRef>,
    stmt: &CppSpanned<CppStmt>,
) -> Result<Vec<Spanned<Stmt>>, Unhandled> {
    match &stmt.value {
        CppStmt::Return(ret) => {
            // A stub generator returns an `AttachDecision`, which is protocol
            // with the dispatcher rather than a value: whether it attached is
            // the dispatcher's business, and `ReturnFromIC` comes from a
            // `writer.returnFromIC()` call, not from returning `Attach`.
            let value = match ctx.unit {
                Unit::StubGenerator { .. } => None,
                _ => ret
                    .value
                    .as_ref()
                    .map(|v| translate_expr(ctx, needed, v))
                    .transpose()?,
            };
            Ok(vec![Spanned::internal(Stmt::Ret(RetStmt {
                value: Spanned::internal(value),
            }))])
        }
        CppStmt::If(s) => Ok(vec![Spanned::internal(Stmt::If(CachetIfStmt {
            cond: Spanned::internal(translate_expr(ctx, needed, &s.cond)?),
            then: translate_block(ctx, needed, &s.then)?,
            // Our `els` is a block, never another `if`, so an `else if` chain
            // comes out as nested `else { if .. }`.
            else_: s
                .els
                .as_ref()
                .map(|els| translate_block(ctx, needed, els).map(ElseClause::Else))
                .transpose()?,
        }))]),
        // Cachet's `let` always binds a value, so a C++ declaration without an
        // initializer -- `Label done;` -- has no counterpart.
        CppStmt::Let(l) => {
            let init = l.init.as_ref().ok_or_else(|| {
                Unhandled::new(format!("declaration of `{}` without an initializer", l.name))
            })?;
            Ok(vec![Spanned::internal(Stmt::Let(LetStmt {
                lhs: LocalVar {
                    ident: Spanned::internal(Ident::from(l.name.clone())),
                    is_mut: false,
                    // Inferred, as the hand-written models leave it.
                    type_: None,
                },
                rhs: Spanned::internal(translate_expr(ctx, needed, init)?),
            }))])
        }
        CppStmt::Assert(_) => Err(Unhandled::new(String::from("assertion"))),
        // `writer.compareDoubleResult(..)` records a CacheIR op, which Cachet
        // spells `emit CacheIR::CompareDoubleResult(..)`. The op name is the
        // method's, capitalized -- both come from the same entry in
        // CacheIROps.yaml. Arguments carry over unchanged.
        //
        // The op's *semantics* live in `CacheIRCompiler::emit<Op>`, which is a
        // separate unit to translate; the call is not chased into it.
        CppStmt::Expr(CppExpr::Call(call)) if writer_call(call).is_some() => {
            let name = writer_call(call).unwrap();
            let args = call
                .args
                .iter()
                .map(|arg| {
                    Ok(Spanned::internal(Arg::Expr(translate_expr(
                        ctx, needed, arg,
                    )?)))
                })
                .collect::<Result<Vec<_>, Unhandled>>()?;
            Ok(vec![Spanned::internal(Stmt::Emit(Call {
                target: Spanned::internal(
                    CachetPath::from_ident("CacheIR").nest(Ident::from(op_ident(name))),
                ),
                args: Spanned::internal(args),
            }))])
        }
        CppStmt::Expr(_) => Err(Unhandled::new(String::from("expression statement"))),
    }
}

/// The C++ lines a span covers.
///
/// Whole lines rather than an exact slice: a clang range ends at the *start* of
/// its last token, so `[start.offset, end.offset)` would cut it short.
fn quote(span: &CppSpan) -> Option<String> {
    let CppSpan::Known { file, start, end } = span else {
        return None;
    };
    let text = std::fs::read_to_string(file).ok()?;
    let lines: Vec<&str> = text
        .lines()
        .skip(start.line.checked_sub(1)? as usize)
        .take((end.line.checked_sub(start.line)? + 1) as usize)
        .collect();
    // Drop the common indentation, which is the C++ nesting, not the statement's.
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    Some(
        lines
            .iter()
            .map(|l| l.get(indent..).unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// An untranslated statement, kept as a comment holding the C++ it stood for.
///
/// The quote spans the whole statement, while the reason names the innermost
/// construct that failed -- quoting that would cut the statement mid-expression.
fn unhandled_comment(e: &Unhandled, stmt: &CppSpan) -> Comment {
    let text = match quote(stmt) {
        Some(cpp) => format!("unhandled {}:\n{cpp}", e.what),
        None => format!("unhandled {}", e.what),
    };
    Comment { text }
}

/// A block of statements. Cachet blocks can also end in a bare tail expression;
/// C++ always returns explicitly, so `value` is always `None`.
///
/// A statement that can't be translated becomes a comment rather than failing
/// the whole body.
fn translate_block(
    ctx: &Ctx,
    needed: &mut Vec<FnRef>,
    body: &CppCompoundStmt,
) -> Result<Block, Unhandled> {
    let mut stmts = Vec::new();
    for stmt in &body.stmts {
        match translate_stmt(ctx, needed, stmt) {
            Ok(translated) => stmts.extend(translated),
            Err(e) => stmts.push(Spanned::internal(Stmt::from(unhandled_comment(
                &e, &stmt.span,
            )))),
        }
    }
    Ok(Block {
        stmts,
        value: Spanned::internal(None),
    })
}

/// A helper the generators call:
///
/// ```text
/// static bool CanConvertToDoubleForToNumber(const Value& v) {
///   return v.isNumber() || v.isBoolean() || v.isNullOrUndefined();
/// }
/// ```
///
/// becomes
///
/// ```text
/// fn CanConvertToDoubleForToNumber(v: Value) -> Bool {
///   return Value::isNumber(v) || Value::isBool(v) || Value::isNullOrUndefined(v);
/// }
/// ```
///
/// Translated rather than modelled: it has no entry in [`translate_method`], so
/// the translation descends into its definition. A helper that *does* have an
/// entry bottoms out there instead, and never needs translating.
pub fn translate_fn_def(fn_def: &FnDef) -> Result<(CallableItem, Vec<FnRef>), Unhandled> {
    // Taking a writer is what makes a function emit, so dropping the parameter
    // is what the `emits` clause replaces.
    let emits = fn_def
        .params
        .iter()
        .any(|param| is_writer(&param.ty))
        .then(|| Spanned::internal(CachetPath::from_ident("CacheIR")));

    let params = fn_def
        .params
        .iter()
        .filter(|param| !is_writer(&param.ty))
        .map(|param| {
            let type_ = translate_type(&param.ty)
                .map_err(|e| Unhandled::new(format!("parameter `{}`: {}", param.name, e.what)))?;
            Ok(CachetParam::Var(VarParam {
                ident: Spanned::internal(Ident::from(param.name.clone())),
                // C++ passes these by value or by const reference, so nothing
                // is written back.
                kind: VarParamKind::In,
                type_: Spanned::internal(type_),
            }))
        })
        .collect::<Result<Vec<_>, Unhandled>>()?;

    let ret =
        translate_type(&fn_def.ret).map_err(|e| Unhandled::new(format!("return type: {}", e.what)))?;

    // A helper is top-level, so it has no parent to qualify names against.
    // `needed` is created here and returned: its scope is this one definition.
    let mut needed = Vec::new();
    let body = translate_block(
        &Ctx {
            unit: Unit::Helper,
        },
        &mut needed,
        &fn_def.body,
    )?;

    let item = CallableItem {
        // Kept verbatim, as field and local names are.
        ident: Spanned::internal(Ident::from(fn_def.name.clone())),
        attrs: Vec::new(),
        is_unsafe: false,
        params,
        emits,
        ret: Some(Spanned::internal(ret)),
        body: Spanned::internal(Some(body)),
    };
    Ok((item, needed))
}

/// `tryAttachNumber` becomes `TryAttachNumber`: Cachet spells ops capitalized,
/// as `emit CacheIR::CompareDoubleResult` in the models does.
fn op_ident(method: &str) -> String {
    let mut chars = method.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `op TryAttachNumber() { <preamble> <body> }`, with the helpers it needs.
fn create_generator_op(
    ctx: &Ctx,
    gen_def: &GenDef,
) -> Result<(CallableItem, Vec<FnRef>), Unhandled> {
    let preamble = translate_preamble(&gen_def.params)?;
    let mut needed = Vec::new();
    let body = translate_block(ctx, &mut needed, &gen_def.body)?;

    let item = CallableItem {
        ident: Spanned::internal(Ident::from(op_ident(&gen_def.method))),
        attrs: Vec::new(),
        is_unsafe: false,
        // No parameters: the operands arrive through the preamble's
        // `defineInputValueId` calls rather than being passed in.
        params: Vec::new(),
        // Inherited from the enclosing `ir`, which already says `emits CacheIR`.
        emits: None,
        // The C++ returns `AttachDecision`, which is the dispatcher's business;
        // an op yields nothing.
        ret: None,
        body: Spanned::internal(Some(Block {
            stmts: preamble.into_iter().chain(body.stmts).collect(),
            value: Spanned::internal(None),
        })),
    };
    Ok((item, needed))
}

/// `CompareIRGenerator::tryAttachNumber` becomes
/// `ir CompareIRGenerator emits CacheIR { .. }`: the generator class names the
/// `ir`, and every stub generator emits CacheIR.
pub fn translate_gen_def(gen_def: &GenDef) -> Result<(IrItem, Vec<FnRef>), Unhandled> {
    // `writer` is how the C++ emits, not state the generator holds: each
    // `writer.foo(..)` becomes an `emit`, so the field itself has no
    // counterpart in the `ir` and is dropped before translating the rest.
    let fields: Vec<Ref> = get_gen_def_fields(&gen_def)
        .into_iter()
        .filter(|field| field.ty.scope != CACHE_IR_WRITER)
        .collect();
    let var_items = create_field_var_items(&fields)?;
    let ctx = Ctx {
        unit: Unit::StubGenerator {
            ir: Ident::from(gen_def.class.clone()),
        },
    };
    let (op, needed) = create_generator_op(&ctx, gen_def)?;
    let generator_op = Item::Op(op);

    // The `var`s first, then the single `op`, as the hand-written models order
    // them: state before the code that reads it.
    let items = var_items
        .into_iter()
        .chain([Spanned::internal(generator_op)])
        .collect();

    Ok((
        IrItem {
            // Spans are `internal` throughout: these nodes are synthesized, so
            // there is no Cachet source location to point at.
            ident: Spanned::internal(Ident::from(gen_def.class.clone())),
            emits: Some(Spanned::internal(CachetPath::from_ident("CacheIR"))),
            items,
        },
        needed,
    ))
}

/// Extraction or translation failed.
#[derive(Debug)]
pub enum Error {
    Extract(SubsetError),
    Unhandled(Unhandled),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Extract(e) => write!(f, "{e}"),
            Error::Unhandled(e) => write!(f, "{e}"),
        }
    }
}

impl From<SubsetError> for Error {
    fn from(e: SubsetError) -> Self {
        Error::Extract(e)
    }
}

impl From<Unhandled> for Error {
    fn from(e: Unhandled) -> Self {
        Error::Unhandled(e)
    }
}

/// A top-level comment, for recording what didn't translate.
fn note(text: String) -> Spanned<Item> {
    Spanned::internal(Item::from(Comment { text }))
}

/// A helper's C++ signature, using the types as C++ spells them.
fn cpp_signature(fn_def: &FnDef) -> String {
    let params: Vec<String> = fn_def
        .params
        .iter()
        .map(|p| format!("{} {}", p.ty.spelled, p.name))
        .collect();
    format!(
        "{} {}({})",
        fn_def.ret.spelled,
        fn_def.name,
        params.join(", ")
    )
}

/// A stub generator and every helper it calls, as one module.
///
/// Helpers come first, then the `ir`. Translation drives the descent: a helper
/// is only translated because emitted code calls it, so a statement that fell
/// back to a comment pulls nothing in.
pub fn translate_generator(generator: &Entity<'_>) -> Result<Mod, Error> {
    let gen_def = get_gen_def(generator)?;
    let (ir, needed) = translate_gen_def(&gen_def)?;

    // Each definition brings its own callees, so deeper helpers stay resolvable.
    let mut callees = gen_def.callees;
    let mut queue: VecDeque<FnRef> = needed.into();
    let mut seen: HashSet<FnId> = HashSet::new();
    let mut helpers = Vec::new();

    while let Some(fn_ref) = queue.pop_front() {
        // Insert before translating, so a cycle terminates instead of looping.
        if !seen.insert(fn_ref.id.clone()) {
            continue;
        }

        // A helper that can't be translated becomes a note rather than failing
        // the module: the generator is still worth seeing. What it leaves behind
        // is a call to a name nothing defines, which the comment accounts for.
        let Some(entity) = callees.get(&fn_ref.id).copied() else {
            helpers.push(note(format!(
                "`{}` has no definition in this translation unit",
                fn_ref.name
            )));
            continue;
        };
        let fn_def = match get_fn_def(&entity) {
            Ok(fn_def) => fn_def,
            Err(e) => {
                helpers.push(note(format!("cannot extract `{}`: {e}", fn_ref.name)));
                continue;
            }
        };
        match translate_fn_def(&fn_def) {
            Ok((item, more)) => {
                helpers.push(Spanned::internal(Item::Fn(item)));
                callees.extend(fn_def.callees);
                queue.extend(more);
            }
            Err(e) => helpers.push(note(format!(
                "cannot translate:\n{}\n{e}",
                cpp_signature(&fn_def)
            ))),
        }
    }

    Ok(helpers
        .into_iter()
        .chain([Spanned::internal(Item::Ir(ir))])
        .collect())
}
