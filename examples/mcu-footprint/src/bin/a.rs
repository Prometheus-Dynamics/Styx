//! Configuration A as a firmware image (see the crate documentation); on the host, one run.

#![cfg_attr(target_os = "none", no_std, no_main)]

#[cfg(target_os = "none")]
#[cortex_m_rt::entry]
fn main() -> ! {
    use styx_mcu_footprint as fp;
    fp::rt::init_heap();
    let (w, h) = fp::QQVGA;
    fp::rt::finish(fp::a::run(w, h, 30).fingerprint)
}

#[cfg(not(target_os = "none"))]
fn main() {
    let (w, h) = styx_mcu_footprint::QQVGA;
    println!("{:?}", styx_mcu_footprint::a::run(w, h, 30));
}
