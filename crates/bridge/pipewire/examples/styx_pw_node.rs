//! Runs the Styx PipeWire node for a while: `styx_pw_node <socket> <node-name> <seconds>`. The
//! producer (`styx-gst`'s `styx_producer` example) may come and go on `<socket>` while it runs;
//! the node waits for it and learns its buffers each time it returns. Status is printed once a
//! second.

use std::time::{Duration, Instant};

use styx_pipewire::{Control, NodeConfig, run};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, socket, name, seconds] = args.as_slice() else {
        eprintln!("usage: styx_pw_node <socket> <node-name> <seconds>");
        std::process::exit(2);
    };
    let config = NodeConfig {
        name: name.clone(),
        socket: socket.into(),
        fps: (30, 1),
    };
    let seconds: u64 = seconds.parse().expect("seconds");
    let control = Control::new();
    println!("styx_pw_node: node {name} from {socket}");
    std::thread::scope(|scope| {
        let node = scope.spawn(|| run(&config, &control));
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(seconds) && !node.is_finished() {
            std::thread::sleep(Duration::from_secs(1));
            let status = control.status();
            println!(
                "styx_pw_node: {:?} connects {} failures {} next {:?} frames {} released {} busy {} last {:?}",
                status.link.state,
                status.link.connects,
                status.link.failures,
                status.link.next_delay,
                status.stats.frames,
                status.stats.released,
                status.stats.busy,
                status.link.last_error,
            );
        }
        control.stop();
        match node.join().expect("node thread") {
            Ok(stats) => println!("styx_pw_node: done {stats:?}"),
            Err(err) => {
                println!("styx_pw_node: error: {err}");
                std::process::exit(1);
            }
        }
    });
}
