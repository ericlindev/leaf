# iOS Packet Tunnel Integration

This repository now supports driving the `tun` inbound from `NEPacketTunnelFlow`
without passing a TUN file descriptor into Rust.

## Why this exists

Apple's `NEPacketTunnelFlow` API is packet-based:

- `readPacketsWithCompletionHandler` delivers raw IP packets to the provider.
- `writePackets:withProtocols:` injects raw IP packets back into the system stack.

That does not match Leaf's previous iOS-only `tun-fd` path, which expected Rust to
own a TUN file descriptor. The new API keeps Leaf's TUN netstack and dispatcher
logic intact, but replaces the device boundary with runtime-scoped packet queues.

## Rust-side behavior

Keep using a normal Leaf `tun` inbound in config. The `tun` inbound is still what
activates the lwIP/smoltcp netstack and fake-DNS handling.

When `leaf_packet_tunnel_init_with_options(rt_id, ...)` is called before `leaf_run*`:

- Leaf uses the external packet queue for that runtime.
- Leaf does not create or consume a TUN file descriptor.
- `tun` device settings such as `fd`, `auto`, `name`, `address`, `gateway`, and
  `netmask` are not used by Rust for that runtime.
- `tun2socks`, `fake_dns_include`, and `fake_dns_exclude` still apply.

If no external packet queue is initialized, Leaf keeps the existing `tun-fd` /
device-backed behavior.

## FFI contract

The new functions are exported from `leaf-ffi`:

- `leaf_packet_tunnel_init_with_options(rt_id, input_queue_size, output_queue_size)`
- `leaf_packet_tunnel_init(rt_id, output_queue_size)`
- `leaf_packet_tunnel_set_input_ready_callback(rt_id, context, callback)`
- `leaf_packet_tunnel_set_output_callback(rt_id, context, callback)`
- `leaf_packet_tunnel_write(rt_id, packet_ptr, packet_len)`
- `leaf_packet_tunnel_read(rt_id, buffer_ptr, buffer_len, packet_len_out)`
- `leaf_packet_tunnel_close(rt_id)`

Important details:

- Call `leaf_packet_tunnel_init_with_options` before `leaf_run`,
  `leaf_run_with_options`, or `leaf_run_with_config_string`.
- Do not call `leaf_packet_tunnel_init*` twice for the same `rt_id` without
  calling `leaf_packet_tunnel_close` first.
- `leaf_packet_tunnel_init` is a convenience wrapper that uses the default
  Swift -> Leaf input queue size from `PACKET_TUNNEL_INPUT_QUEUE_SIZE`.
- `input_queue_size` and `output_queue_size` must each be less than or equal to
  `MAX_PACKET_TUNNEL_QUEUE_SIZE` (`4096`) when passed explicitly.
- Passing `0` uses the configured default queue size and clamps it to
  `MAX_PACKET_TUNNEL_QUEUE_SIZE` if the backing env var is larger.
- `leaf_packet_tunnel_write` returns `ERR_QUEUE_FULL` when the bounded
  Swift -> Leaf ingress queue is saturated.
- `leaf_packet_tunnel_write` returns `ERR_IO` after the tunnel has been closed.
  Treat that as a terminal teardown signal, not as retryable backpressure.
- Register `leaf_packet_tunnel_set_input_ready_callback` if you buffer pending
  packets on the Swift side. That callback fires when Rust consumes at least one
  packet after the ingress queue has been full.
- The output callback is only a wake-up signal. It fires when Leaf's outbound
  packet queue transitions from empty to non-empty, and it may also fire
  immediately when you register it if packets are already queued.
- The callback may run on an arbitrary Rust runtime thread. Dispatch back onto
  your provider queue before touching provider state.
- `leaf_packet_tunnel_read` returns `ERR_NO_DATA` when the queue is empty.
- If the read buffer is too small, `leaf_packet_tunnel_read` returns
  `ERR_BUFFER_TOO_SMALL`, sets `packet_len_out` to the required size, and keeps
  the packet queued for the next read attempt.
- Passing `buffer = nil` and `bufferLen = 0` to `leaf_packet_tunnel_read` is a
  supported probe that returns the required packet size without consuming it.

## Swift integration pattern

Use one queue for all interactions with the packet tunnel provider. Keep a small
local backlog for packets that could not be written to Rust because
`leaf_packet_tunnel_write` returned `ERR_QUEUE_FULL`, and only re-arm
`readPackets` when that backlog has been drained.

```swift
import NetworkExtension

final class LeafPacketPump {
    private let rtId: UInt16
    private let providerQueue = DispatchQueue(label: "leaf.packet.provider")
    private unowned let provider: NEPacketTunnelProvider
    private var scratch = [UInt8](repeating: 0, count: 65535)
    private var pendingInput = [Data]()
    private var isReading = false

    init(rtId: UInt16, provider: NEPacketTunnelProvider) {
        self.rtId = rtId
        self.provider = provider
    }

    func start() throws {
        guard leaf_packet_tunnel_init_with_options(rtId, 256, 256) == ERR_OK else {
            throw PumpError.initFailed
        }

        let ctx = Unmanaged.passUnretained(self).toOpaque()
        let inputRc = leaf_packet_tunnel_set_input_ready_callback(rtId, ctx) { _, context in
            let pump = Unmanaged<LeafPacketPump>.fromOpaque(context!).takeUnretainedValue()
            pump.providerQueue.async {
                pump.flushPendingInput()
                pump.scheduleReadLoopIfNeeded()
            }
        }
        guard inputRc == ERR_OK else {
            throw PumpError.callbackFailed
        }

        let rc = leaf_packet_tunnel_set_output_callback(rtId, ctx) { _, context in
            let pump = Unmanaged<LeafPacketPump>.fromOpaque(context!).takeUnretainedValue()
            pump.providerQueue.async {
                pump.drainLeafOutput()
            }
        }
        guard rc == ERR_OK else {
            throw PumpError.callbackFailed
        }

        scheduleReadLoopIfNeeded()
    }

    func stop() {
        leaf_packet_tunnel_close(rtId)
    }

    private func scheduleReadLoopIfNeeded() {
        guard !isReading, pendingInput.isEmpty else { return }
        isReading = true
        provider.packetFlow.readPackets { [weak self] packets, _ in
            guard let self else { return }
            self.providerQueue.async {
                self.isReading = false
                self.pendingInput.append(contentsOf: packets)
                self.flushPendingInput()
                self.scheduleReadLoopIfNeeded()
            }
        }
    }

    private func flushPendingInput() {
        while let packet = pendingInput.first {
            let rc = packet.withUnsafeBytes { rawBuffer -> Int32 in
                guard let base = rawBuffer.bindMemory(to: UInt8.self).baseAddress else {
                    return ERR_INVALID_ARGUMENT
                }
                return leaf_packet_tunnel_write(self.rtId, base, rawBuffer.count)
            }

            if rc == ERR_OK {
                pendingInput.removeFirst()
                continue
            }
            if rc == ERR_QUEUE_FULL {
                return
            }

            // ERR_IO after close/teardown is terminal, not retryable backpressure.
            pendingInput.removeAll()
            return
        }
    }

    private func drainLeafOutput() {
        var packets = [Data]()
        var protocols = [NSNumber]()

        while true {
            var packetLen: Int = 0
            let rc = leaf_packet_tunnel_read(rtId, &scratch, scratch.count, &packetLen)
            if rc == ERR_NO_DATA {
                break
            }
            if rc != ERR_OK {
                break
            }

            let data = Data(bytes: scratch, count: packetLen)
            let family: Int32 = ((data.first ?? 0) >> 4) == 6 ? AF_INET6 : AF_INET
            packets.append(data)
            protocols.append(NSNumber(value: family))
        }

        if !packets.isEmpty {
            _ = provider.packetFlow.writePackets(packets, withProtocols: protocols)
        }
    }
}
```

## Operational notes

- Do not call `readPackets` again while you still have pending packets buffered
  on the Swift side.
- Treat `ERR_QUEUE_FULL` as backpressure, not as a fatal error.
- Treat `ERR_IO` from `leaf_packet_tunnel_write` as a terminal closed/teardown
  state and stop retrying input writes.
- The output callback should only schedule work; do not perform heavy work inline.
- The input-ready callback should only schedule a retry of your buffered input.
- A 65535-byte scratch buffer is enough for one IP packet read from Leaf.
- Use a stable `rt_id` for the lifetime of one provider instance.
- Call `leaf_packet_tunnel_close(rt_id)` during teardown even though runtime
  shutdown also cleans up the packet tunnel.
