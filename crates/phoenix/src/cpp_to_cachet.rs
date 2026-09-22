use std::fmt;
use std::path::Path;

use cachet_lang::ast::{Ident, Path as CachetPath, Spanned};
use cachet_lang::parser::{
    Block, Call, CallableItem, Expr, GlobalVarItem, IrItem, Item, LetStmt, LocalVar, Stmt,
};
use clang::{Clang, Index};

use crate::cpp_subset::{Indirection, Param, RefKind, Type as CppType, walk_block};
use crate::{
    clang_utils::{find_definition, get_errors, parse_file},
    cpp_subset::{GenDef, Ref, Visit, get_gen_def},
};

/// A C++ construct with no Cachet counterpart yet.
///
/// Translation refuses rather than guesses: a type mapped wrongly would verify
/// something other than the code that runs, which is worse than not verifying.
#[derive(Clone, Debug)]
pub struct Unhandled(pub String);

impl fmt::Display for Unhandled {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "unhandled {}", self.0)
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
            return Err(Unhandled(format!(
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

        _ => Err(Unhandled(format!(
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
        return Err(Unhandled(format!(
            "parameter `{}`: expected a ValOperandId, found `{}`",
            other.name, other.ty.spelled
        )));
    }
    if params.len() != 2 {
        return Err(Unhandled(format!(
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
                .map_err(|e| Unhandled(format!("field `{}`: {}", field.name, e.0)))?;
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

fn to_cachet(
    index: &Index,
    gen_path: &Path,
    db: &Path,
    gen_name: &String,
) -> Result<Vec<Item>, String> {
    let tu = parse_file(index, gen_path, db);
    let errors = get_errors(&tu);
    if !errors.is_empty() {
        return Err(String::from("has errors")); // TODO make better
    }

    let Some(def) = find_definition(tu.get_entity(), gen_name) else {
        return Err(String::from("could not find definition")); // TODO make better
    };

    let Ok(extracted) = get_gen_def(&def) else {
        return Err(String::from("could not extract def")); // TODO make better
    };

    Err(String::from("ni"))
}
