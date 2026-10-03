# Fuzzing

cargo-fuzz targets for every parser of untrusted bytes in Styx: see
[docs/fuzzing.md](../docs/fuzzing.md) for the targets, how to run them (`scripts/fuzz.sh`) and how
to add one. `seeds/` holds committed seed inputs, `dicts/` the dictionaries; `corpus/` and
`artifacts/` are local.
