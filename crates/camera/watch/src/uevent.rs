//! The kernel's uevent messages, parsed. A message is `ACTION@DEVPATH` then `KEY=value` pairs,
//! each ended by a NUL byte. Only the kernel's own messages count (see [`crate::v4l2`]).

/// What the kernel did to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Add,
    Remove,
    /// Anything else (`change`, `bind`, `move`, ...): not a camera appearing or leaving.
    Other,
}

/// The fields of a uevent that the watcher uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uevent {
    pub action: Action,
    /// `DEVPATH`, for example `/devices/platform/vivid.0/video4linux/video1`.
    pub devpath: String,
    /// `SUBSYSTEM`.
    pub subsystem: String,
    /// `DEVNAME` (a node name under `/dev`, for example `video1`), when the kernel gives one.
    pub devname: Option<String>,
}

/// Parses one kernel uevent. `None` when the message is not a uevent (no header, or no
/// `ACTION`, `DEVPATH` or `SUBSYSTEM`).
pub fn parse_uevent(message: &[u8]) -> Option<Uevent> {
    let mut fields = message
        .split(|&b| b == 0)
        .filter(|field| !field.is_empty())
        .filter_map(|field| std::str::from_utf8(field).ok());
    // The header is `ACTION@DEVPATH`; the kernel's `KEY=value` lines follow it.
    let header = fields.next()?;
    if !header.contains('@') {
        return None;
    }
    let (mut action, mut devpath, mut subsystem, mut devname) = (None, None, None, None);
    for field in fields {
        if let Some((key, value)) = field.split_once('=') {
            match key {
                "ACTION" => action = Some(value),
                "DEVPATH" => devpath = Some(value),
                "SUBSYSTEM" => subsystem = Some(value),
                "DEVNAME" => devname = Some(value),
                _ => {}
            }
        }
    }
    let action = match action? {
        "add" => Action::Add,
        "remove" => Action::Remove,
        _ => Action::Other,
    };
    Some(Uevent {
        action,
        devpath: devpath?.to_string(),
        subsystem: subsystem?.to_string(),
        devname: devname.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Recorded in the kernel's format (`add@DEVPATH` then the environment), as the kernel sends
    // them on the kernel's netlink group.
    const ADD_VIVID: &[u8] = b"add@/devices/platform/vivid.0/video4linux/video1\0\
ACTION=add\0DEVPATH=/devices/platform/vivid.0/video4linux/video1\0SUBSYSTEM=video4linux\0\
DEVNAME=video1\0MAJOR=81\0MINOR=1\0SEQNUM=2140\0";
    const REMOVE_VIVID: &[u8] = b"remove@/devices/platform/vivid.0/video4linux/video1\0\
ACTION=remove\0DEVPATH=/devices/platform/vivid.0/video4linux/video1\0SUBSYSTEM=video4linux\0\
DEVNAME=video1\0MAJOR=81\0MINOR=1\0SEQNUM=2141\0";
    const BIND_USB: &[u8] = b"bind@/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0\0\
ACTION=bind\0DEVPATH=/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0\0SUBSYSTEM=usb\0\
DRIVER=uvcvideo\0SEQNUM=2142\0";

    #[test]
    fn parses_add_and_remove_of_a_video_node() {
        let add = parse_uevent(ADD_VIVID).expect("add");
        assert_eq!(add.action, Action::Add);
        assert_eq!(add.subsystem, "video4linux");
        assert_eq!(add.devname.as_deref(), Some("video1"));
        assert_eq!(add.devpath, "/devices/platform/vivid.0/video4linux/video1");
        let remove = parse_uevent(REMOVE_VIVID).expect("remove");
        assert_eq!(remove.action, Action::Remove);
        assert_eq!(remove.devname, add.devname);
    }

    #[test]
    fn other_actions_are_not_arrivals_or_departures() {
        assert_eq!(parse_uevent(BIND_USB).expect("bind").action, Action::Other);
    }

    #[test]
    fn messages_that_are_not_uevents_are_refused() {
        assert_eq!(parse_uevent(b"libudev\0"), None);
        assert_eq!(parse_uevent(b""), None);
        assert_eq!(
            parse_uevent(b"add@/devices/x\0SUBSYSTEM=video4linux\0"),
            None,
            "no ACTION"
        );
        assert_eq!(parse_uevent(&[0xff, 0x00, 0x41]), None, "not UTF-8");
    }
}
