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

```sh
# typed AST
cargo run -- 'SetPropIRGenerator::tryAttachNativeSetSlot'

# call graph, following definitions inside the CacheIR sources 3 levels deep
cargo run -- 'SetPropIRGenerator::tryAttachNativeSetSlot' --calls --depth 3

# the generator lowered into the modeled C++ subset
cargo run -- 'CompareIRGenerator::tryAttachNumber' --subset

# a different file / compile database
cargo run -- 'CallIRGenerator::tryAttachArrayPush' --source js/src/jit/CacheIR.cpp --db path/to/compile_commands.json
```

phoenix loads libclang at run time from `~/.mozbuild/clang`, so the parser and
the compile flags come from the same toolchain. `PHOENIX_CLANG_ARGS` appends
extra flags to the parse (e.g. `-v` to print the header search path).
