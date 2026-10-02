# Functional correctness specs

`notes/` proves CacheIR stubs are **safe**. This directory proves one is
**right**: that the `Value` left in the output register is the one the baseline
interpreter would have pushed.

```sh
./notes-correctness/check.sh                      # spec + three controls
./notes-correctness/compile-verify.sh compare_int32
```

## What is here

| file | |
|---|---|
| `semantics.cachet` | reference semantics for JS operations, modeled from the C++ interpreter |
| `stubs/compare_int32.cachet` | functional spec for `CompareIRGenerator::tryAttachInt32` |
| `compile-verify.sh` | compile one spec and run Corral on it |
| `check.sh` | verify the spec, then three mutations that must fail |

## How a functional spec is written

Three pieces, all of which already existed in the model:

- `CacheIR::getOperandLocation(id)` + `MASM::getValue` turns a `ValOperandId`
  into the symbolic `Value` the interpreter would have popped off the stack.
- `CacheIR::AssertEqValueOutput(v)` checks the output register against `v`.
- Both are `emit`ted into the instruction stream, so they are evaluated during
  the *interpreter* phase, while the expected value is computed during the
  *generator* phase. That is what makes the comparison happen at the right
  point in time.

The only genuinely new thing is `semantics.cachet`.

## How `semantics.cachet` is written

Each `fn` is **definitionally complete on the cases a stub's guards can admit**
and bottoms out in an uninterpreted `...Slow` helper everywhere else. A guarded
path reduces concretely; an unguarded one reaches an opaque function and proves
nothing. This keeps the specs small without letting the verifier conclude
anything false.

Nothing in a spec states how a guard corresponds to an interpreter path. You
write both sides -- the reference semantics as a total function, the guards as
hypotheses -- and the solver discovers that they coincide.

Cachet has no recursion, so the recursive steps of the spec algorithms are
unrolled by hand where a guarded path needs them.

## Result: the elided assertions are the functional preconditions

`tryAttachInt32` carries two `MOZ_ASSERT`s that phoenix currently drops as
"unhandled assertion". Dropping them is sound for everything Icarus verifies
today. Both are **required** for functional correctness, and `check.sh` removes
each in turn and shows the proof fail:

| assertion | counterexample if dropped |
|---|---|
| `MOZ_ASSERT_IF(lhsVal_.isNull() \|\| rhsVal_.isNull(), !IsEqualityOp(op_))` | `null == 0` -- spec says false (null is loosely equal only to null/undefined); the stub converts null to int32 `0` and answers true |
| `MOZ_ASSERT_IF(op_ == StrictEq \|\| op_ == StrictNe, lhsVal_.type() == rhsVal_.type())` | `true === 1` -- spec says false (different types); the stub converts the boolean to int32 `1` and answers true |

Note the asymmetry these two encode. For *relational* ops the conversion is
correct -- `null < 1` really is `0 < 1` -- which is why `CanConvertToInt32ForToNumber`
admits null at all. The assertions are the generator's record of *why* the fast
path is valid, and safety verification never needed them.

So the elided-assertion list in `crates/phoenix/docs/next-steps.md` is not a
tidiness item. It is where the functional preconditions live.

## Measured

Corral, on this machine:

```
compare_int32 (full spec, all 8 JSOps, Int32/Bool/Null operands)   verifies,  9.4 s
  int32 x int32 only                                               verifies,  3.1 s
  bool  x bool  only                                               verifies,  3.6 s
  bool  x int32, loose equality or relational                      verifies
  bool  x int32, strict equality, without the type assertion       counterexample
```

The `negated_postcond` control -- asserting the negation of the expected value
-- fails, which is what rules out the spec being vacuously true.

## Not done

- Only `Int32`/`Bool`/`Null` operands. `Double`, `String`, `BigInt`, `Symbol`
  and `Object` all reach the uninterpreted tail of `looselyEqual`.
- Only the result is specified. Nothing says the stub leaves the rest of the
  heap alone -- which is vacuous here (this stub touches no heap) but is the
  substance of a spec for `StoreFixedSlot`.
- `semantics.cachet` was written by hand from the C++. Deriving it the way
  phoenix derives generators is the obvious next question.
