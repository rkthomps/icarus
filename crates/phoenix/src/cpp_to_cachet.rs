use std::fmt;
use std::path::Path;

use cachet_lang::ast::{
    BinOper, CompareBinOper, Ident, LogicalBinOper, Path as CachetPath, Spanned,
};
use cachet_lang::ast::{NegateKind, VarParamKind};
use cachet_lang::parser::{
    Arg, BinOperExpr, Block, Call, CallableItem, Expr, GlobalVarItem, IrItem, Item, LetStmt,
    LocalVar, NegateExpr, Param as CachetParam, RetStmt, Stmt, VarParam,
};
use clang::{Clang, Index};

use crate::cpp_subset::{
    Callee as CppCallee, CompoundStmt as CppCompoundStmt, Expr as CppExpr, FnDef, Indirection,
    Param, RefKind, Span as CppSpan, Spanned as CppSpanned, Stmt as CppStmt, Type as CppType,
    walk_block,
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
        (ty, "isBoolean") if ty == value => "isBool",
        (ty, "isNullOrUndefined") if ty == value => "isNullOrUndefined",
        _ => return None,
    };
    // `impl Value { fn isNumber(value: Value) }` is called as
    // `Value::isNumber(v)`, so the C++ receiver becomes the first argument.
    Some(recv_ty.nest(Ident::from(name)))
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
fn translate_expr(expr: &CppSpanned<CppExpr>) -> Result<Expr, Unhandled> {
    translate_expr_value(expr).map_err(|e| match e.span {
        CppSpan::Unknown => e.at(&expr.span),
        _ => e,
    })
}

fn translate_expr_value(expr: &CppSpanned<CppExpr>) -> Result<Expr, Unhandled> {
    match &expr.value {
        CppExpr::Ref(r) => match r.kind {
            RefKind::Param | RefKind::Local => Ok(Expr::Var(Spanned::internal(
                CachetPath::from_ident(Ident::from(r.name.clone())),
            ))),
            // A field needs the enclosing `ir` to qualify it, as
            // `CompareIRGenerator::op_`, which an expression alone doesn't know.
            RefKind::Field => Err(Unhandled::new(format!("field reference `{}`", r.name))),
        },

        CppExpr::Call(call) => match &call.callee {
            CppCallee::Method {
                recv: Some(recv),
                name,
            } => {
                let recv_ty = translate_type(named_type(recv)?)?;
                let target = translate_method(recv_ty, name)
                    .ok_or_else(|| Unhandled::new(format!("method `{name}`")))?;
                // The receiver leads, then the C++ arguments.
                let mut args = vec![Spanned::internal(Arg::Expr(translate_expr(recv)?))];
                for arg in &call.args {
                    args.push(Spanned::internal(Arg::Expr(translate_expr(arg)?)));
                }
                Ok(Expr::Invoke(Call {
                    target: Spanned::internal(target),
                    args: Spanned::internal(args),
                }))
            }
            CppCallee::Method { recv: None, name } => {
                Err(Unhandled::new(format!("method `{name}` on an implicit `this`")))
            }
            CppCallee::Free(name) => Err(Unhandled::new(format!("call to `{name}`"))),
        },

        CppExpr::Unary(unary) => {
            let kind = translate_unary_oper(&unary.op)
                .ok_or_else(|| Unhandled::new(format!("unary `{}`", unary.op)))?;
            Ok(Expr::Negate(Box::new(NegateExpr {
                kind: Spanned::internal(kind),
                expr: Spanned::internal(translate_expr(&unary.operand)?),
            })))
        }

        CppExpr::Binary(binary) => {
            let oper = translate_bin_oper(&binary.op)
                .ok_or_else(|| Unhandled::new(format!("binary `{}`", binary.op)))?;
            Ok(Expr::BinOper(Box::new(BinOperExpr {
                oper: Spanned::internal(oper),
                lhs: Spanned::internal(translate_expr(&binary.lhs)?),
                rhs: Spanned::internal(translate_expr(&binary.rhs)?),
            })))
        }

        CppExpr::Construct(c) => Err(Unhandled::new(format!("construction of `{}`", c.ty.spelled))),
        CppExpr::EnumConst(e) => Err(Unhandled::new(format!("enum constant `{}::{}`", e.ty, e.name))),
        CppExpr::Lit(_) => Err(Unhandled::new(String::from("literal"))),
        CppExpr::This => Err(Unhandled::new(String::from("`this`"))),
    }
}

/// One C++ statement can yield several, so this returns a list.
fn translate_stmt(stmt: &CppSpanned<CppStmt>) -> Result<Vec<Spanned<Stmt>>, Unhandled> {
    translate_stmt_values(stmt).map_err(|e| match e.span {
        CppSpan::Unknown => e.at(&stmt.span),
        _ => e,
    })
}

fn translate_stmt_values(stmt: &CppSpanned<CppStmt>) -> Result<Vec<Spanned<Stmt>>, Unhandled> {
    match &stmt.value {
        CppStmt::Return(ret) => {
            let value = ret.value.as_ref().map(translate_expr).transpose()?;
            Ok(vec![Spanned::internal(Stmt::Ret(RetStmt {
                value: Spanned::internal(value),
            }))])
        }
        CppStmt::If(_) => Err(Unhandled::new(String::from("if statement"))),
        CppStmt::Let(l) => Err(Unhandled::new(format!("declaration of `{}`", l.name))),
        CppStmt::Assert(_) => Err(Unhandled::new(String::from("assertion"))),
        CppStmt::Expr(_) => Err(Unhandled::new(String::from("expression statement"))),
    }
}

/// A block of statements. Cachet blocks can also end in a bare tail expression;
/// C++ always returns explicitly, so `value` is always `None`.
fn translate_block(body: &CppCompoundStmt) -> Result<Block, Unhandled> {
    let mut stmts = Vec::new();
    for stmt in &body.stmts {
        stmts.extend(translate_stmt(stmt)?);
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
pub fn translate_fn_def(fn_def: &FnDef) -> Result<CallableItem, Unhandled> {
    let params = fn_def
        .params
        .iter()
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

    Ok(CallableItem {
        // Kept verbatim, as field and local names are.
        ident: Spanned::internal(Ident::from(fn_def.name.clone())),
        attrs: Vec::new(),
        is_unsafe: false,
        params,
        // A plain helper emits nothing.
        emits: None,
        ret: Some(Spanned::internal(ret)),
        body: Spanned::internal(Some(translate_block(&fn_def.body)?)),
    })
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

/// `op TryAttachNumber() { <preamble> }`.
fn create_generator_op(gen_def: &GenDef) -> Result<CallableItem, Unhandled> {
    let preamble = translate_preamble(&gen_def.params)?;

    Ok(CallableItem {
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
            // TODO: the translated `gen_def.body`, after the preamble.
            stmts: preamble,
            value: Spanned::internal(None),
        })),
    })
}

/// `CompareIRGenerator::tryAttachNumber` becomes
/// `ir CompareIRGenerator emits CacheIR { .. }`: the generator class names the
/// `ir`, and every stub generator emits CacheIR.
pub fn translate_gen_def(gen_def: GenDef) -> Result<IrItem, Unhandled> {
    // `writer` is how the C++ emits, not state the generator holds: each
    // `writer.foo(..)` becomes an `emit`, so the field itself has no
    // counterpart in the `ir` and is dropped before translating the rest.
    let fields: Vec<Ref> = get_gen_def_fields(&gen_def)
        .into_iter()
        .filter(|field| field.ty.scope != CACHE_IR_WRITER)
        .collect();
    let var_items = create_field_var_items(&fields)?;
    let generator_op = Item::Op(create_generator_op(&gen_def)?);

    // The `var`s first, then the single `op`, as the hand-written models order
    // them: state before the code that reads it.
    let items = var_items
        .into_iter()
        .chain([Spanned::internal(generator_op)])
        .collect();

    Ok(IrItem {
        // Spans are `internal` throughout: these nodes are synthesized, so
        // there is no Cachet source location to point at.
        ident: Spanned::internal(Ident::from(gen_def.class)),
        emits: Some(Spanned::internal(CachetPath::from_ident("CacheIR"))),
        items,
    })
}
