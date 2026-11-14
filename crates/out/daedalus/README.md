# styx-daedalus

Styx frames as Daedalus values: a `styx:frame` value in the dma-buf memory location, moved by the
planner's transfers, and read from the shared ring by a graph in another process. The `daedalus`
feature is off by default.

## Fault codes

A Styx step that faults gives a fault code in Styx's own space, from `styx_daedalus::fault`
(`FaultCode`). The code is opaque to Daedalus; a caller matches on it instead of parsing text. The
underlying error goes in the step's detail, with its words complete. The numbers are published once
and never change or are reused.

| Code | Name | Meaning |
|---|---|---|
| 0 | Other | A fault Styx has no code for. The detail says what it was. Never given by a Styx check. |
| 1 | LayoutRefused | The shared-memory layout of the other side is not this build's, so the attach is refused. The detail names the expected and found layout words. |
| 2 | OrionRefused | Orion refused a revision or a feature. The detail names the revision and the feature as Orion says them. |
| 3 | StreamNotMapped | A step needed the buffer's bytes on the CPU, and the buffer is not mapped for the CPU (GPU memory, or a dma-buf not mapped here). |
| 4 | StreamReadOnly | A step needed to write the buffer, and its mapping is read-only (or it has none that writes). |
| 5 | NoGpuAccess | No GPU adapter was found, and this user is refused the GPU's device nodes. The detail names the nodes and the group that owns them. |
| 6 | DeviceFailed | A device failed: no device opened, a job was refused or failed, or the device is gone. The detail carries the device's own words. |
| 7 | NoDataPath | A frame has neither a dma-buf nor CPU bytes, so there is nothing a device or a step can read. |
| 8 | UnsupportedFormat | A step takes one pixel format and the frame is another. The detail names both. |
| 9 | PlaneLayoutMismatch | The buffer is smaller than the frame's planes need for its format and size, or the planes can't be laid out. The detail gives the sizes. |
| 10 | OrionRevision | Orion refused a node's revision. The detail names the node and the revision it is, and the revision this build is. |
| 11 | OrionFeature | Orion refused a feature at the level it is asked for. The detail names the node, the feature and the level it needs. |
| 12 | ContractMismatch | The board's Lemnos and this Styx are different builds: a device's contract is laid out another way here, so the bind is refused. The detail names the contract and both base hashes. |
Checked by `tests/fault_codes.rs`: the table, the enum and this file cannot drift apart.
