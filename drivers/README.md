# sctl Comms Plugins

Comms providers are C-ABI shared libraries loaded by `sctl` when a target has hardware-specific communications needs. The main server keeps the HTTP/MCP/API surface and generic AT transport; plugins own device-specific detection, polling, control, and recovery.

The first provider is [`sctl-comms-quectel`](sctl-comms-quectel/), which builds `libsctl_comms_quectel.so` for the current Quectel AT-command LTE/GNSS path. Future 5G, satellite, robotics radio, or space-compute link providers should implement the ABI in [`../crates/sctl-comms-abi`](../crates/sctl-comms-abi/). Providers can be written in Rust, C, Zig, C++, or any language that can export the v1 C ABI.
