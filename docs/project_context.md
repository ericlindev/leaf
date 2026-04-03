# Leaf Project Context

This document records the working context for the `leaf` repository in its
current state, with emphasis on the iOS `NetworkExtension` packet-tunnel use
case.

## 1. Repository purpose

Leaf is a Rust proxy framework with:

- multiple inbound protocols
- multiple outbound protocols
- routing, DNS, fake-DNS, and health checking
- transparent proxy support through TUN and Windows NF

For the iOS use case, the relevant part of the system is the `tun` inbound,
which terminates raw IP packets into an in-process TCP/UDP netstack and then
forwards extracted streams/datagrams through Leaf's dispatcher and outbound
pipeline.

## 2. Workspace layout

Top-level crates:

- `leaf/`
  The core library and most protocol/runtime logic.
- `leaf-cli/`
  CLI wrapper around the core.
- `leaf-ffi/`
  C ABI surface for mobile and native embedding.
- `leaf-plugins/`
  Optional protocol/plugin implementations.

Important docs/scripts:

- [README.md](/Users/evan/Desktop/leaf/README.md)
- [scripts/apple_common.sh](/Users/evan/Desktop/leaf/scripts/apple_common.sh)
- [docs/ios_packet_tunnel_integration.md](/Users/evan/Desktop/leaf/docs/ios_packet_tunnel_integration.md)

## 3. Core runtime architecture

Entry point:

- [lib.rs](/Users/evan/Desktop/leaf/leaf/src/lib.rs)

Startup flow:

1. Parse config from file/string/internal model.
2. Initialize logging.
3. Create Tokio runtime.
4. Build DNS client, outbound manager, router, stat manager, dispatcher, NAT manager.
5. Build inbound listeners through `InboundManager`.
6. Spawn inbound runners and auxiliary tasks.
7. Run until shutdown / ctrl-c / runtime completion.

Key runtime components:

- `Dispatcher`
  Routes accepted streams/datagrams to the selected outbound handlers.
- `NatManager`
  Maintains UDP session mapping between inbound datagrams and outbound datagrams.
- `Router`
  Applies rules to select outbound tags.
- `DnsClient`
  Handles DNS resolution and feeds fake-DNS/routing needs.
- `InboundManager`
  Materializes listeners/runners for each configured inbound.

## 4. Important source areas

Core runtime:

- [lib.rs](/Users/evan/Desktop/leaf/leaf/src/lib.rs)
- [app/inbound/manager.rs](/Users/evan/Desktop/leaf/leaf/src/app/inbound/manager.rs)
- [app/dispatcher.rs](/Users/evan/Desktop/leaf/leaf/src/app/dispatcher.rs)
- [app/nat_manager.rs](/Users/evan/Desktop/leaf/leaf/src/app/nat_manager.rs)
- [app/router.rs](/Users/evan/Desktop/leaf/leaf/src/app/router.rs)
- [app/dns/client.rs](/Users/evan/Desktop/leaf/leaf/src/app/dns/client.rs)

Transparent proxy / TUN:

- [proxy/tun/inbound.rs](/Users/evan/Desktop/leaf/leaf/src/proxy/tun/inbound.rs)
- [proxy/tun/mod.rs](/Users/evan/Desktop/leaf/leaf/src/proxy/tun/mod.rs)
- [app/inbound/tun_listener.rs](/Users/evan/Desktop/leaf/leaf/src/app/inbound/tun_listener.rs)

Configuration:

- [config/common.rs](/Users/evan/Desktop/leaf/leaf/src/config/common.rs)
- [config/conf/config.rs](/Users/evan/Desktop/leaf/leaf/src/config/conf/config.rs)
- [config/internal/config.proto](/Users/evan/Desktop/leaf/leaf/src/config/internal/config.proto)

Mobile / FFI:

- [leaf-ffi/src/lib.rs](/Users/evan/Desktop/leaf/leaf-ffi/src/lib.rs)
- [leaf/src/mobile/mod.rs](/Users/evan/Desktop/leaf/leaf/src/mobile/mod.rs)
- [leaf/src/mobile/callback.rs](/Users/evan/Desktop/leaf/leaf/src/mobile/callback.rs)

## 5. Configuration model

Leaf supports at least:

- `.conf` format
- `.json` format

The `tun` inbound is represented in the internal config model by
`TunInboundSettings` in [config.proto](/Users/evan/Desktop/leaf/leaf/src/config/internal/config.proto).

Relevant `tun` fields:

- `fd`
- `auto`
- `name`
- `address`
- `gateway`
- `netmask`
- `mtu`
- `fake_dns_exclude`
- `fake_dns_include`
- `tun2socks`
- `wintun`
- `dns_servers`

Historically on iOS, the path used `fd` to pass a TUN fd into Rust.

## 6. Existing TUN architecture

The `tun` inbound does not itself proxy packets directly to the network. It does:

1. read raw IP packets from a device-like source
2. feed them into one of two user-space netstacks:
   - `netstack-lwip`
   - `netstack-smoltcp`
3. extract TCP streams and UDP datagrams from that netstack
4. hand them to Leaf's `Dispatcher` / `NatManager`
5. receive response packets from the netstack and write them back to the device-like source

That makes the device boundary the correct abstraction point for iOS packet-flow
integration.

## 7. iOS-specific problem statement

Apple `NEPacketTunnelProvider` exposes:

- `readPacketsWithCompletionHandler`
- `writePackets:withProtocols:`

through `NEPacketTunnelFlow`, not a reusable TUN fd that Rust can own in the
same way as Linux/macOS/Android.

Because of that, the old iOS strategy of feeding `tun-fd` into Leaf is not the
best fit for a production `PacketTunnelProvider`.

## 8. New iOS packet-tunnel architecture

New code added:

- [proxy/tun/packet_io.rs](/Users/evan/Desktop/leaf/leaf/src/proxy/tun/packet_io.rs)

This introduces a runtime-scoped external packet transport:

- ingress queue: Swift -> Leaf
- egress queue: Leaf -> Swift
- ingress-capacity callback to wake Swift when Rust can accept packets again
- output-ready callback to wake Swift when outbound packets are available

Queue sizing:

- default Swift -> Leaf queue size comes from `PACKET_TUNNEL_INPUT_QUEUE_SIZE`
- default Leaf -> Swift queue size comes from `NETSTACK_OUTPUT_CHANNEL_SIZE`
- both queues are capped at `MAX_PACKET_TUNNEL_QUEUE_SIZE` (`4096`)
- explicit queue sizes above the cap are rejected; default-derived values are clamped

Runtime ownership:

- packet transports are keyed by `rt_id`
- they are initialized before `leaf_run*`
- the `tun` inbound claims the transport during startup
- runtime teardown removes the transport

### New FFI functions

Defined in [leaf-ffi/src/lib.rs](/Users/evan/Desktop/leaf/leaf-ffi/src/lib.rs):

- `leaf_packet_tunnel_init`
- `leaf_packet_tunnel_init_with_options`
- `leaf_packet_tunnel_set_input_ready_callback`
- `leaf_packet_tunnel_set_output_callback`
- `leaf_packet_tunnel_write`
- `leaf_packet_tunnel_read`
- `leaf_packet_tunnel_close`

### Error codes added

- `ERR_INVALID_ARGUMENT`
- `ERR_ALREADY_EXISTS`
- `ERR_NOT_FOUND`
- `ERR_BUFFER_TOO_SMALL`
- `ERR_QUEUE_FULL`

## 9. How the iOS packet path now works

When a packet tunnel is initialized for the runtime:

1. `leaf_run*` starts normally.
2. `InboundManager` creates the `tun` listener with the runtime id.
3. `proxy::tun::inbound::new` checks whether an external packet tunnel exists.
4. If present, it uses that transport instead of calling `tun::create_as_async`.
5. The rest of the pipeline remains unchanged:
   - lwIP/smoltcp netstack
   - TCP extraction
   - UDP NAT handling
   - fake-DNS integration
   - dispatcher/outbounds

This preserves almost all pre-existing behavior while replacing only the packet
I/O boundary.

## 10. Current iOS integration contract

See [ios_packet_tunnel_integration.md](/Users/evan/Desktop/leaf/docs/ios_packet_tunnel_integration.md).

Expected Swift provider behavior:

1. Call `leaf_packet_tunnel_init_with_options` or `leaf_packet_tunnel_init`.
2. Register the input-ready and output-ready callbacks.
3. Start Leaf.
4. Feed packets from `packetFlow.readPackets` into `leaf_packet_tunnel_write`.
   If it returns `ERR_QUEUE_FULL`, stop re-arming `readPackets`, keep a bounded
   local backlog in Swift, and wait for the input-ready callback before retrying.
   If it returns `ERR_IO`, treat the tunnel as closed and stop retrying input writes.
5. In the callback, drain `leaf_packet_tunnel_read` until `ERR_NO_DATA`.
6. Inject drained packets through `packetFlow.writePackets`.
7. Close the packet tunnel during teardown.

## 11. Build context

Core Cargo features:

- default core build uses `default-aws-lc`
- `inbound-tun` enables:
  - `tun`
  - `netstack-lwip`
  - `netstack-smoltcp`
  - `pnet_datalink`

Apple build helpers:

- [scripts/apple_common.sh](/Users/evan/Desktop/leaf/scripts/apple_common.sh)
- [scripts/build_ios_xcframework.sh](/Users/evan/Desktop/leaf/scripts/build_ios_xcframework.sh)
- [scripts/build_apple_xcframework.sh](/Users/evan/Desktop/leaf/scripts/build_apple_xcframework.sh)

Current `leaf-ffi` specifics:

- [leaf-ffi/Cargo.toml](/Users/evan/Desktop/leaf/leaf-ffi/Cargo.toml)
- [leaf-ffi/build.rs](/Users/evan/Desktop/leaf/leaf-ffi/build.rs)

`leaf-ffi/build.rs` adds a `__chkstk_darwin` stub for Apple iOS targets to avoid
an arm64 iPhone device linker failure observed with the current crypto/toolchain
combination. This is a symbol-resolution workaround and should be revisited when
the upstream linker issue is resolved.

## 12. Verification that has been run

Host verification:

- `cargo test -p leaf packet_io::tests --lib`
- `cargo test -p leaf-ffi --lib`

Apple target verification:

- `cargo build -p leaf-ffi --target aarch64-apple-ios-sim --no-default-features --features default-aws-lc`
- `cargo build -p leaf-ffi --target aarch64-apple-ios --no-default-features --features default-aws-lc`

## 13. Review status (2026-04-02)

Reviewed areas:

- runtime-scoped packet transport ownership
- `tun` inbound selection between external packet I/O and device-backed TUN
- FFI packet ABI behavior and error mapping
- runtime teardown / startup failure cleanup
- Apple iOS build/link behavior for `leaf-ffi`

Review conclusion:

- The packet-based iOS integration is structurally sound and keeps Leaf's existing
  TUN/netstack architecture intact.
- The startup cleanup path and short-buffer read path behave correctly in the
  current implementation.
- The Swift -> Rust ingress path is now bounded and exposes backpressure through
  `ERR_QUEUE_FULL` plus an input-ready callback.
- The FFI health-check and activity helpers now use current-thread Tokio runtimes
  and return `ERR_INVALID_ARGUMENT` for invalid outbound tags.
- `leaf_run_with_config_string` keeps the legacy `ERR_CONFIG_PATH` error contract
  for null/invalid config C strings even though the argument is inline content.

## 14. Known strengths of the current design

- Preserves the existing TUN netstack architecture instead of creating an iOS-only fork.
- Keeps fake-DNS, NAT handling, routing, and outbound behavior unchanged.
- Avoids depending on `tun-fd` as the primary iOS integration model.
- Keeps the Swift/Rust interface small and packet-based.
- Supports both lwIP and smoltcp backends.

## 15. Known risks / open technical concerns

1. The output callback is edge-triggered, not level-triggered.
   Swift must drain until `ERR_NO_DATA`. If it stops early, no second callback is
   guaranteed until the queue becomes empty and then non-empty again.

2. The input-ready and output-ready callbacks may execute on arbitrary Rust
   runtime threads.
   Swift must marshal back to its own queue before touching provider state.

3. The current Swift example is a reference pattern, not production app code.
   A real app should keep its own backlog bounded, add structured logging, and
   add teardown guards around the packet pump.

4. There is no dedicated integration test that drives the FFI from Swift or
   Objective-C inside an actual `NetworkExtension` host.

5. The synchronous FFI health-check/activity helpers build a short-lived
   current-thread Tokio runtime per call. This is acceptable for diagnostics,
   but callers should avoid treating them as a high-frequency polling API.

## 16. Recommended next steps

1. Build a real `NEPacketTunnelProvider` wrapper that owns:
   - packet read loop
   - packet write drain loop
   - retry/backpressure queue
   - runtime lifecycle
2. Add integration tests around the FFI packet tunnel ABI.
3. Add operational logging/metrics for:
   - packets queued to Rust
   - packets queued to Swift
   - queue saturation
   - callback wakeups
4. Generate and validate the final Apple XCFramework after the embedding app is wired.

## 17. Files most relevant for future work

If you continue iOS work, start here:

- [proxy/tun/packet_io.rs](/Users/evan/Desktop/leaf/leaf/src/proxy/tun/packet_io.rs)
- [proxy/tun/inbound.rs](/Users/evan/Desktop/leaf/leaf/src/proxy/tun/inbound.rs)
- [leaf-ffi/src/lib.rs](/Users/evan/Desktop/leaf/leaf-ffi/src/lib.rs)
- [docs/ios_packet_tunnel_integration.md](/Users/evan/Desktop/leaf/docs/ios_packet_tunnel_integration.md)
- [docs/project_context.md](/Users/evan/Desktop/leaf/docs/project_context.md)
