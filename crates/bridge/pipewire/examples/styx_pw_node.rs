//! Runs the Styx PipeWire node for a while: `styx_pw_node <socket> <node-name> <buffers> <seconds>`.
//! The producer (`styx-gst`'s `styx_producer` example) must be running on `<socket>`, with the
//! same buffer count.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use styx_pipewire::{NodeConfig, run};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, socket, name, buffers, seconds] = args.as_slice() else {
        eprintln!("usage: styx_pw_node <socket> <node-name> <buffers> <seconds>");
        std::process::exit(2);
    };
    let config = NodeConfig {
        name: name.clone(),
        socket: socket.into(),
        buffers: buffers.parse().expect("buffers"),
        fps: (30, 1),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let timer = {
        let stop = stop.clone();
        let seconds: u64 = seconds.parse().expect("seconds");
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(seconds));
            stop.store(true, Ordering::Release);
        })
    };
    println!("styx_pw_node: node {name} from {socket}, {buffers} buffers");
    let result = run(&config, &stop);
    let _ = timer.join();
    match result {
        Ok(stats) => println!("styx_pw_node: done {stats:?}"),
        Err(err) => {
            println!("styx_pw_node: error: {err}");
            std::process::exit(1);
        }
    }
}
