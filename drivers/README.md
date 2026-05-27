# sctl Comms Providers

Comms providers are external helper binaries launched by `sctl` when a target has hardware-specific communications needs. The main server keeps the HTTP/MCP/API surface; helpers own device-specific detection, polling, control, and recovery.

The first provider is [`sctl-comms-quectel`](sctl-comms-quectel/), which supports the current Quectel AT-command LTE/GNSS path. Future 5G, satellite, robotics radio, or space-compute link providers should implement the shared protocol in [`../crates/sctl-comms-protocol`](../crates/sctl-comms-protocol/).
