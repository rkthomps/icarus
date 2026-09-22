# phoenix

Dumps the type-resolved clang AST (or call graph) of a CacheIR stub generator,
as a first step toward compiling stub generators to Icarus automatically.

## Setup

```sh
cargo build -p phoenix
```

That's it. `build.rs` does the SpiderMonkey setup on first build and skips it
afterwards:

1. shallow-clones mozilla-central at a pinned revision into
   `../mozilla-central` (sibling of this repo; ~2 GB)
2. `mach bootstrap` — downloads Mozilla's vendored clang + libclang into
   `~/.mozbuild/clang` (~1 GB)
3. `mach configure` — debug JS-shell-only build into `obj-js`
4. `mach build pre-export export` — generates headers `CacheIR.cpp` includes
5. `mach build-backend -b CompileDB` — writes `obj-js/compile_commands.json`

No C++ is compiled. First build takes ~10 minutes, nearly all download.
Cargo hides build-script output; progress goes to `cargo:warning` lines and the
full log is in `target/debug/build/phoenix-*/out/setup.log`.

Requirements: `git`, and a Python 3.10–3.14 on `PATH` for `mach`.

Overrides:

| variable | effect |
|---|---|
| `PHOENIX_MOZ_CENTRAL` | checkout location |
| `PHOENIX_MOZ_REV` | mozilla-central revision (default pinned in `build.rs`) |
| `PHOENIX_SKIP_SETUP=1` | skip setup entirely; pass `--db` at run time |
| `MOZBUILD_STATE_PATH` | mach's toolchain dir (default `~/.mozbuild`) |
| `LIBCLANG_PATH` | use a different libclang at run time |

The vendored toolchain for the Firefox-93-era `icarus-firefox` tree has expired
upstream, which is why a current mozilla-central is used as the parsing target.

## Run

Four subcommands, each taking a `Class::method` (or a bare function name).
Note the `-p phoenix`: the workspace has several binaries, so a bare
`cargo run --` can't tell which one you mean.

```sh
# translate to Cachet -- the main path
cargo run -p phoenix -- cachet 'CompareIRGenerator::tryAttachNumber'

# ...and write it where the verify scripts look for it
cargo run -p phoenix -- cachet 'CompareIRGenerator::tryAttachNumber' \
  --out notes/stubs/compare-number.cachet

# the generator lowered into the modeled C++ subset
cargo run -p phoenix -- subset 'CompareIRGenerator::tryAttachNumber'

# the raw, fully type-resolved clang AST
cargo run -p phoenix -- ast 'InlinableNativeIRGenerator::tryAttachArrayPush'

# call graph, following definitions inside the CacheIR sources 3 levels deep
cargo run -p phoenix -- calls 'SetPropIRGenerator::tryAttachNativeSetSlot' --depth 3
```

`--source` and `--db` are global and may go before or after the subcommand.
`--source` is matched as a path suffix against the compile database, and
defaults to `js/src/jit/CacheIR.cpp`; reach for it when the symbol lives
elsewhere, such as the machine-code side of an op:

```sh
cargo run -p phoenix -- subset 'CacheIRCompiler::emitCompareDoubleResult' \
  --source js/src/jit/CacheIRCompiler.cpp

cargo run -p phoenix -- ast 'CompareIRGenerator::tryAttachNumber' \
  --db path/to/compile_commands.json
```

`cargo run -p phoenix -- help` lists the subcommands; `help <subcommand>` shows
one in detail.

phoenix loads libclang at run time from `~/.mozbuild/clang`, so the parser and
the compile flags come from the same toolchain. `PHOENIX_CLANG_ARGS` appends
extra flags to the parse (e.g. `-v` to print the header search path).
