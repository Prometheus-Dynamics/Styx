//! `styx-algo`'s integration tests in one binary: one module per former `tests/*.rs` file, so an
//! edit relinks one test binary instead of 9 (docs/development.md#the-gate).

mod common;

mod awb_continuity;
mod replay;
mod sim_ae;
mod sim_af;
mod sim_awb;
mod sim_deflicker;
mod sim_flicker;
mod sim_start;
mod tuning_rpi;
