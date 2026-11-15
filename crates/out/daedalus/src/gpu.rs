//! The GPU device Styx opens for Daedalus, and the status lines a device that didn't open, or a
//! node that faulted, reads as in Styx's own logs.

use daedalus::layers::host::FaultDetail;
use daedalus_wgpu::{DEFAULT_KEPT_IMPORTS, OpenError, Options, WgpuBackend};

/// What Styx sets on a Daedalus GPU device. Everything not named here is Daedalus's default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuOptions {
    /// The adapter, by name, as [`Options::adapter`] takes it. `None` picks the default.
    pub adapter: Option<String>,
    /// How many dma-buf imports each import step keeps ([`Options::kept_imports`]). A producer that
    /// cycles through more buffers than this imports each again when it comes round; 0 keeps
    /// none.
    pub kept_imports: usize,
}

impl Default for GpuOptions {
    fn default() -> Self {
        Self {
            adapter: None,
            kept_imports: DEFAULT_KEPT_IMPORTS,
        }
    }
}

impl GpuOptions {
    /// Daedalus's options for a device opened with these.
    pub fn to_wgpu(&self) -> Options {
        Options {
            adapter: self.adapter.clone(),
            kept_imports: self.kept_imports,
            ..Options::default()
        }
    }

    /// The backend for a device with these options, not yet started. Add the value types it moves
    /// (`with_dmabuf::<StyxFrame>()`) and then start it.
    pub fn backend(&self) -> WgpuBackend {
        WgpuBackend::open(self.to_wgpu())
    }
}

/// A fault's reason as Styx shows it: the reason, and ` (cut)` after it when the body's reason was
/// longer than a fault keeps, so that the start shown is not taken for the whole.
pub fn fault_text(reason: &str, reason_cut: bool) -> String {
    if reason_cut {
        format!("{reason} (cut)")
    } else {
        reason.to_string()
    }
}

/// A fault's code as Styx shows it: the node's own number, or `no code` for a node that gave none,
/// and Daedalus's own codes say so.
pub fn fault_code_text(code: u32, own_code: bool) -> String {
    match (code, own_code) {
        (0, false) => "no code".to_string(),
        (code, false) => format!("code {code}"),
        (code, true) => format!("code {code} (Daedalus's own)"),
    }
}

/// A fault as Styx's status line shows it: the code, the reason with its cut flag, then the step's
/// detail (the host's [`FaultDetail`], when the step wrote one) with its own cut flag. Line breaks
/// in the detail are shown as ` / `, so a multi-line detail stays one line.
pub fn fault_status(
    code: u32,
    own_code: bool,
    reason: &str,
    reason_cut: bool,
    detail: Option<FaultDetail<'_>>,
) -> String {
    let mut line = format!(
        "{}: {}",
        fault_code_text(code, own_code),
        fault_text(reason, reason_cut)
    );
    if let Some(detail) = detail {
        let text = detail.text.trim_end().replace('\n', " / ");
        line.push_str("; detail: ");
        line.push_str(&fault_text(&text, detail.cut));
    }
    line
}

/// The status line for a GPU that did not open, with the group a refused node belongs to looked up
/// by `group_of` (see [`group_of_node`]).
pub fn open_error_status_with(
    error: &OpenError,
    group_of: impl Fn(&str) -> Option<String>,
) -> String {
    match error {
        OpenError::NoAccess(nodes) => {
            let mut groups: Vec<String> = nodes.split(", ").filter_map(&group_of).collect();
            groups.sort();
            groups.dedup();
            let group = if groups.is_empty() {
                "(unknown)".to_string()
            } else {
                groups.join(", ")
            };
            format!("GPU not usable: user lacks group {group} for {nodes}")
        }
        other => format!("GPU not usable: {other}"),
    }
}

/// The status line for a GPU that did not open, naming the group of any refused node.
pub fn open_error_status(error: &OpenError) -> String {
    open_error_status_with(error, group_of_node)
}

/// The group that owns a device node, by its name in `/etc/group`. Daedalus names the refused nodes
/// and not the group, so the group is the node's own: `None` when the node can't be read, and the
/// number when no group has it.
pub fn group_of_node(node: &str) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let gid = std::fs::metadata(node).ok()?.gid();
    let groups = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let named = groups.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next()?;
        (fields.next()?.parse::<u32>().ok()? == gid).then(|| name.to_string())
    });
    Some(named.unwrap_or_else(|| format!("gid {gid}")))
}
