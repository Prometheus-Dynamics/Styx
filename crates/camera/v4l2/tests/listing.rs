//! Listing and probing without opening: `probe()` reads sysfs and opens no node; a named probe opens
//! exactly that one node. Counted through an injected [`Opener`], so the test sees every open.
//!
//! The fake-sysfs test runs everywhere. The machine tests use the real nodes when they exist (a vivid
//! capture node by its sysfs name, and a virtual loopback node that must never be opened) and print
//! why they skip otherwise.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use styx_lemnos::v4l2::v4l2::VideoDevice;
use styx_v4l2::Error;
use styx_v4l2::{Opener, Prober, System};

/// An opener that records every path it is asked to open, then opens it for real.
#[derive(Clone, Default)]
struct Counting {
    opens: Arc<Mutex<Vec<(PathBuf, bool)>>>,
}

impl Counting {
    fn opened(&self) -> Vec<(PathBuf, bool)> {
        self.opens.lock().unwrap().clone()
    }
}

impl Opener for Counting {
    fn open(&self, path: &Path, write: bool) -> Result<VideoDevice, Error> {
        self.opens.lock().unwrap().push((path.to_path_buf(), write));
        System.open(path, write)
    }
}

/// A fake sysfs class directory: `video2` is virtual (loopback driver), `video10` is physical and
/// bound to `uvcvideo`. Their device links point into the fixture, so nothing real is touched.
fn fake_sysfs(test: &str) -> (PathBuf, PathBuf) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("listing-fake-{test}"));
    let _ = fs::remove_dir_all(&root);
    let sys = root.join("sys/class/video4linux");
    let dev = root.join("dev");
    let drivers = root.join("drivers");
    for dir in [&sys, &dev, &drivers] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::create_dir_all(drivers.join("v4l2loopback")).unwrap();
    fs::create_dir_all(drivers.join("uvcvideo")).unwrap();
    let node = |name: &str, index: &str, name_file: &str, driver: &str| {
        let dir = sys.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("name"), format!("{name_file}\n")).unwrap();
        fs::write(dir.join("index"), format!("{index}\n")).unwrap();
        let device = root.join(format!("devices/{name}"));
        fs::create_dir_all(&device).unwrap();
        symlink(drivers.join(driver), device.join("driver")).unwrap();
        symlink(&device, dir.join("device")).unwrap();
        fs::write(dev.join(name), "").unwrap();
    };
    node("video2", "0", "OBS Virtual Camera", "v4l2loopback");
    node("video10", "0", "Integrated Camera", "uvcvideo");
    (sys, dev)
}

#[test]
fn listing_reads_sysfs_and_opens_nothing() {
    let (sys, dev) = fake_sysfs("list");
    let counting = Counting::default();
    let listing = Prober::new(counting.clone(), &sys, &dev).list();

    assert!(
        counting.opened().is_empty(),
        "listing opened {:?}",
        counting.opened()
    );
    let names: Vec<_> = listing
        .nodes
        .iter()
        .map(|n| n.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["video2", "video10"], "natural order by number");

    let virt = &listing.nodes[0];
    assert!(
        virt.virtual_node,
        "a loopback driver marks the node virtual"
    );
    assert_eq!(virt.name, "OBS Virtual Camera");
    assert_eq!(virt.index, Some(0));
    assert_eq!(virt.driver.as_deref(), Some("v4l2loopback"));
    assert!(!virt.opened, "listing never opens a node");
    assert_eq!(
        virt.queue, None,
        "the queue is unknown until the node is probed"
    );

    let physical = &listing.nodes[1];
    assert!(!physical.virtual_node);
    assert_eq!(physical.driver.as_deref(), Some("uvcvideo"));
    assert!(
        physical.device_path.is_some(),
        "the parent device is listed"
    );
    assert!(!physical.opened);
}

#[test]
fn a_named_probe_opens_exactly_one_node() {
    // A probe of a node that is not a capture node (or cannot open here) is still one open: the
    // fake node has no device behind it, so the open itself fails, and the count is what matters.
    let (sys, dev) = fake_sysfs("named");
    let counting = Counting::default();
    let prober = Prober::new(counting.clone(), &sys, &dev);
    let _ = prober.probe_card("Integrated Camera");
    let opened = counting.opened();
    assert_eq!(opened.len(), 1, "one open for one named probe: {opened:?}");
    assert_eq!(opened[0].0, dev.join("video10"));
    assert!(!opened[0].1, "a full probe opens read-only");
}

#[test]
fn a_virtual_node_is_never_opened_by_listing_or_card_lookup_of_another_node() {
    let (sys, dev) = fake_sysfs("virtual");
    let counting = Counting::default();
    let prober = Prober::new(counting.clone(), &sys, &dev);
    let _ = prober.list();
    let _ = prober.probe_card("Integrated Camera");
    assert!(
        counting
            .opened()
            .iter()
            .all(|(path, _)| path != &dev.join("video2")),
        "the virtual node was opened: {:?}",
        counting.opened()
    );
}

#[test]
fn machine_listing_opens_nothing_and_names_the_vivid_node() {
    let counting = Counting::default();
    let listing = Prober::new(counting.clone(), "/sys/class/video4linux", "/dev").list();
    assert!(counting.opened().is_empty(), "listing opened a node");
    for node in &listing.nodes {
        assert!(!node.opened);
        if node.virtual_node {
            println!(
                "virtual {} ({}), not opened",
                node.path.display(),
                node.name
            );
        }
    }
    let Some(vivid) = listing
        .nodes
        .iter()
        .find(|n| !n.virtual_node && n.name.starts_with("vivid") && n.name.contains("vid-cap"))
    else {
        println!("SKIP vivid probe: no vivid capture node by sysfs name on this machine");
        return;
    };
    println!(
        "vivid by sysfs name: {} at {}",
        vivid.name,
        vivid.path.display()
    );
    assert!(counting.opened().is_empty());
}

#[test]
fn a_named_probe_of_the_vivid_node_opens_only_that_node() {
    let counting = Counting::default();
    let prober = Prober::new(counting.clone(), "/sys/class/video4linux", "/dev");
    let listing = prober.list();
    let Some(vivid) = listing
        .nodes
        .iter()
        .find(|n| !n.virtual_node && n.name.starts_with("vivid") && n.name.contains("vid-cap"))
        .map(|n| n.name.clone())
    else {
        println!("SKIP named probe: no vivid capture node by sysfs name on this machine");
        return;
    };
    let device = prober
        .probe_card(&vivid)
        .expect("probe the vivid node by name")
        .expect("vivid is a capture node");
    let opened = counting.opened();
    assert_eq!(opened.len(), 1, "one open for one named probe: {opened:?}");
    assert_eq!(opened[0].0, device.path);
    assert!(
        listing
            .nodes
            .iter()
            .filter(|n| n.virtual_node)
            .all(|n| opened.iter().all(|(p, _)| p != &n.path)),
        "a virtual node was opened"
    );
    println!(
        "named probe {vivid}: card={:?} formats={} controls={}",
        device.card,
        device.formats.len(),
        device.raw_controls.len()
    );
}

#[test]
fn a_non_default_input_is_selected_and_its_formats_are_listed() {
    let counting = Counting::default();
    let prober = Prober::new(counting.clone(), "/sys/class/video4linux", "/dev");
    let Some(path) = prober
        .list()
        .nodes
        .into_iter()
        .find(|n| !n.virtual_node && n.name.starts_with("vivid") && n.name.contains("vid-cap"))
        .map(|n| n.path)
    else {
        println!("SKIP input: no vivid capture node on this machine");
        return;
    };
    let default = prober
        .probe_node(&path)
        .expect("default probe")
        .expect("capture node");
    let selected = match prober.probe_input(&path, 1) {
        Ok(Some(device)) => device,
        Ok(None) => {
            println!("SKIP input: {} is not a capture node", path.display());
            return;
        }
        Err(e) => {
            println!(
                "SKIP input: input 1 is not selectable on {}: {e}",
                path.display()
            );
            return;
        }
    };
    assert_eq!(
        default.input, None,
        "the default probe keeps the driver's input"
    );
    assert_eq!(selected.input, Some(1));
    assert!(!selected.formats.is_empty(), "input 1 lists its formats");
    println!(
        "input 1 on {}: {} formats (default input: {})",
        path.display(),
        selected.formats.len(),
        default.formats.len()
    );
}
