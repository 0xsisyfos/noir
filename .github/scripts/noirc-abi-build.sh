#!/bin/bash
set -eu

.github/scripts/wasm-bindgen-install.sh
.github/scripts/wasm-opt-install.sh
yarn workspace @noir-lang/noirc_abi build
