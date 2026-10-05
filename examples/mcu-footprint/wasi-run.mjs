// Runs a wasm32-wasip1 test binary under Node's WASI, for the heap test's 32-bit numbers
// (docs/mcu.md):
//   CARGO_TARGET_WASM32_WASIP1_RUNNER="node examples/mcu-footprint/wasi-run.mjs" \
//       cargo test -p styx-mcu-footprint --release --target wasm32-wasip1 --test heap -- --nocapture
import { WASI } from 'node:wasi';
import { readFileSync } from 'node:fs';

const [, , file, ...args] = process.argv;
const wasi = new WASI({ version: 'preview1', args: [file, ...args], env: process.env, preopens: {} });
const wasm = await WebAssembly.compile(readFileSync(file));
const instance = await WebAssembly.instantiate(wasm, wasi.getImportObject());
process.exitCode = wasi.start(instance);
