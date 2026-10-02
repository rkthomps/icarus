#!/bin/bash
set -euo pipefail

# Compile and verify a functional-correctness stub spec from
# notes-correctness/stubs/. Mirrors scripts/compile-mac.sh + verify-mac.sh,
# but rooted here so these specs stay separate from the safety ones.

sample_name="${1}"
shift

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
repo_dir="$(cd -- "${here}" &> /dev/null && cd .. && pwd)"

cachet_file="${here}/stubs/${sample_name}.cachet"
support_bpl_file="${repo_dir}/notes/support.bpl"

out_dir="${here}/out"
mkdir -p "${out_dir}"
bpl_file="${out_dir}/${sample_name}.bpl"

cd "${repo_dir}"
cargo build --quiet --bin cachet-compiler --bin bpl-tree-shaker --bin bpl-inliner

cargo run --quiet --bin cachet-compiler -- "${cachet_file}" \
  --cpp-decls "${out_dir}/${sample_name}.h" \
  --cpp-defs "${out_dir}/${sample_name}.inc" \
  --bpl "${bpl_file}"
cat "${support_bpl_file}" "${bpl_file}" | sponge "${bpl_file}"
cargo run --quiet --bin bpl-tree-shaker -- -i "${bpl_file}" -t '#JSOp' -p '#MASM^Op'
cargo run --quiet --bin bpl-inliner -- -i "${bpl_file}" -p 9999

"${repo_dir}/scripts/run-corral-mac.sh" "${bpl_file}" ${@+"$@"}
