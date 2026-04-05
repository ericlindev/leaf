#![allow(clippy::missing_safety_doc)]
use std::{
    ffi::{c_void, CStr, CString},
    future::Future,
    os::raw::c_char,
    panic::{self, AssertUnwindSafe},
    slice,
};

/// No error.
pub const ERR_OK: i32 = 0;
/// Config path error.
pub const ERR_CONFIG_PATH: i32 = 1;
/// Config parsing error.
pub const ERR_CONFIG: i32 = 2;
/// IO error.
pub const ERR_IO: i32 = 3;
/// Config file watcher error.
pub const ERR_WATCHER: i32 = 4;
/// Async channel send error.
pub const ERR_ASYNC_CHANNEL_SEND: i32 = 5;
/// Sync channel receive error.
pub const ERR_SYNC_CHANNEL_RECV: i32 = 6;
/// Runtime manager error.
pub const ERR_RUNTIME_MANAGER: i32 = 7;
/// No associated config file.
pub const ERR_NO_CONFIG_FILE: i32 = 8;
/// No data found.
pub const ERR_NO_DATA: i32 = 9;
/// Invalid argument.
pub const ERR_INVALID_ARGUMENT: i32 = 10;
/// Resource already exists.
pub const ERR_ALREADY_EXISTS: i32 = 11;
/// Resource not found.
pub const ERR_NOT_FOUND: i32 = 12;
/// Provided buffer is too small.
pub const ERR_BUFFER_TOO_SMALL: i32 = 13;
/// Packet queue is full, caller must retry or drop according to its policy.
pub const ERR_QUEUE_FULL: i32 = 14;
/// Maximum accepted size for either packet tunnel queue.
pub const MAX_PACKET_TUNNEL_QUEUE_SIZE: usize = 4_096;

// Keep the FFI-exported constant and the runtime implementation in lockstep.
#[allow(dead_code)]
fn assert_max_packet_tunnel_queue_size_matches_runtime() {
    let _: [(); MAX_PACKET_TUNNEL_QUEUE_SIZE] =
        [(); leaf::proxy::tun::packet_io::MAX_PACKET_TUNNEL_QUEUE_SIZE];
}

fn to_errno(e: leaf::Error) -> i32 {
    match e {
        leaf::Error::Config(..) => ERR_CONFIG,
        leaf::Error::NoConfigFile => ERR_NO_CONFIG_FILE,
        leaf::Error::Io(..) => ERR_IO,
        #[cfg(feature = "auto-reload")]
        leaf::Error::Watcher(..) => ERR_WATCHER,
        leaf::Error::AsyncChannelSend(..) => ERR_ASYNC_CHANNEL_SEND,
        leaf::Error::SyncChannelRecv(..) => ERR_SYNC_CHANNEL_RECV,
        leaf::Error::RuntimeManager => ERR_RUNTIME_MANAGER,
    }
}

fn io_to_errno(e: std::io::Error) -> i32 {
    match e.kind() {
        std::io::ErrorKind::InvalidInput => ERR_INVALID_ARGUMENT,
        std::io::ErrorKind::AlreadyExists => ERR_ALREADY_EXISTS,
        std::io::ErrorKind::NotFound => ERR_NOT_FOUND,
        std::io::ErrorKind::WouldBlock => ERR_QUEUE_FULL,
        _ => ERR_IO,
    }
}

unsafe fn parse_string_arg(
    ptr: *const c_char,
    null_err: i32,
    utf8_err: i32,
) -> Result<String, i32> {
    if ptr.is_null() {
        return Err(null_err);
    }
    unsafe { CStr::from_ptr(ptr).to_str() }
        .map(|value| value.to_string())
        .map_err(|_| utf8_err)
}

// These FFI helpers are synchronous and may be called from arbitrary host threads.
// Use a short-lived current-thread runtime per call rather than borrowing Leaf's
// internal runtime or introducing extra worker threads for one-shot diagnostics.
fn block_on_current_thread<F>(future: F) -> Result<F::Output, i32>
where
    F: Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map(|runtime| runtime.block_on(future))
        .map_err(io_to_errno)
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

fn ffi_boundary<F>(name: &'static str, f: F) -> i32
where
    F: FnOnce() -> i32,
{
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(payload) => {
            tracing::error!("{} panicked: {}", name, panic_message(payload));
            ERR_IO
        }
    }
}

fn callback_cstring(value: &str) -> Option<CString> {
    match CString::new(value) {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::warn!(
                "outbound tag contains an interior NUL byte; replacing it before invoking callback"
            );
            CString::new(value.replace('\0', "\u{FFFD}")).ok()
        }
    }
}

/// Starts leaf with options, on a successful start this function blocks the current
/// thread.
///
/// @note This is not a stable API, parameters will change from time to time.
///
/// @param rt_id A unique ID to associate this leaf instance, this is required when
///              calling subsequent FFI functions, e.g. reload, shutdown.
/// @param config_path The path of the config file, must be a file with suffix .conf
///                    or .json, according to the enabled features.
/// @param auto_reload Enabls auto reloading when config file changes are detected,
///                    takes effect only when the "auto-reload" feature is enabled.
/// @param multi_thread Whether to use a multi-threaded runtime.
/// @param auto_threads Sets the number of runtime worker threads automatically,
///                     takes effect only when multi_thread is true.
/// @param threads Sets the number of runtime worker threads, takes effect when
///                     multi_thread is true, but can be overridden by auto_threads.
/// @param stack_size Sets stack size of the runtime worker threads, takes effect when
///                   multi_thread is true.
/// @return ERR_OK on finish running, any other errors means a startup failure.
#[no_mangle]
#[allow(unused_variables)]
pub unsafe extern "C" fn leaf_run_with_options(
    rt_id: u16,
    config_path: *const c_char,
    auto_reload: bool, // requires this parameter anyway
    multi_thread: bool,
    auto_threads: bool,
    threads: i32,
    stack_size: i32,
) -> i32 {
    let config_path =
        match unsafe { parse_string_arg(config_path, ERR_CONFIG_PATH, ERR_CONFIG_PATH) } {
            Ok(config_path) => config_path,
            Err(err) => return err,
        };
    if let Err(e) = leaf::util::run_with_options(
        rt_id,
        config_path,
        #[cfg(feature = "auto-reload")]
        auto_reload,
        multi_thread,
        auto_threads,
        threads as usize,
        stack_size as usize,
    ) {
        return to_errno(e);
    }
    ERR_OK
}

/// Starts leaf with a single-threaded runtime, on a successful start this function
/// blocks the current thread.
///
/// @param rt_id A unique ID to associate this leaf instance, this is required when
///              calling subsequent FFI functions, e.g. reload, shutdown.
/// @param config_path The path of the config file, must be a file with suffix .conf
///                    or .json, according to the enabled features.
/// @return ERR_OK on finish running, any other errors means a startup failure.
#[no_mangle]
pub unsafe extern "C" fn leaf_run(rt_id: u16, config_path: *const c_char) -> i32 {
    let config_path =
        match unsafe { parse_string_arg(config_path, ERR_CONFIG_PATH, ERR_CONFIG_PATH) } {
            Ok(config_path) => config_path,
            Err(err) => return err,
        };
    let opts = leaf::StartOptions {
        config: leaf::Config::File(config_path),
        #[cfg(feature = "auto-reload")]
        auto_reload: false,
        runtime_opt: leaf::RuntimeOption::SingleThread,
    };
    if let Err(e) = leaf::start(rt_id, opts) {
        return to_errno(e);
    }
    ERR_OK
}

/// Starts leaf with a single-threaded runtime using an inline config string.
///
/// @param rt_id A unique ID to associate this leaf instance.
/// @param config UTF-8 config content.
///               For compatibility with the existing file-based APIs, a null pointer
///               or invalid UTF-8 still returns `ERR_CONFIG_PATH`.
/// @return ERR_OK on finish running, any other errors mean startup failure.
#[no_mangle]
pub unsafe extern "C" fn leaf_run_with_config_string(rt_id: u16, config: *const c_char) -> i32 {
    let config = match unsafe { parse_string_arg(config, ERR_CONFIG_PATH, ERR_CONFIG_PATH) } {
        Ok(config) => config,
        Err(err) => return err,
    };
    let opts = leaf::StartOptions {
        config: leaf::Config::Str(config),
        #[cfg(feature = "auto-reload")]
        auto_reload: false,
        runtime_opt: leaf::RuntimeOption::SingleThread,
    };
    if let Err(e) = leaf::start(rt_id, opts) {
        return to_errno(e);
    }
    ERR_OK
}

/// Reloads DNS servers, outbounds and routing rules from the config file.
///
/// @param rt_id The ID of the leaf instance to reload.
///
/// @return Returns ERR_OK on success.
#[no_mangle]
pub extern "C" fn leaf_reload(rt_id: u16) -> i32 {
    if let Err(e) = leaf::reload(rt_id) {
        return to_errno(e);
    }
    ERR_OK
}

/// Shuts down leaf.
///
/// @param rt_id The ID of the leaf instance to reload.
///
/// @return Returns true on success, false otherwise.
#[no_mangle]
pub extern "C" fn leaf_shutdown(rt_id: u16) -> bool {
    leaf::shutdown(rt_id)
}

/// Returns true if a leaf instance with the given runtime ID is currently running.
///
/// This is a lightweight, side-effect-free liveness check.
/// It reflects runtime-manager registration state and must not be treated as a
/// full readiness guarantee for packet I/O or startup completion.
///
/// @param rt_id The ID of the leaf instance to query.
/// @return Returns true if the instance is running, false otherwise.
#[no_mangle]
pub extern "C" fn leaf_is_running(rt_id: u16) -> bool {
    leaf::is_running(rt_id)
}

/// Initializes a runtime-scoped external packet tunnel for the TUN inbound.
///
/// When this is set up before `leaf_run*`, Leaf will use packet queues instead of
/// creating/owning a TUN file descriptor for that runtime.
///
/// @param rt_id The runtime ID that will be passed to `leaf_run*`.
/// @param input_queue_size Maximum number of packets queued from Swift to Leaf.
///                         Pass 0 to use the default queue size, clamped to the maximum.
/// @param output_queue_size Maximum number of packets queued from Leaf to Swift.
///                          Pass 0 to use the default queue size, clamped to the maximum.
/// Both queues must be less than or equal to `MAX_PACKET_TUNNEL_QUEUE_SIZE`.
/// @return Returns ERR_OK on success.
#[no_mangle]
pub extern "C" fn leaf_packet_tunnel_init_with_options(
    rt_id: u16,
    input_queue_size: usize,
    output_queue_size: usize,
) -> i32 {
    match leaf::proxy::tun::packet_io::init_runtime_packet_tunnel(
        rt_id,
        input_queue_size,
        output_queue_size,
    ) {
        Ok(()) => ERR_OK,
        Err(e) => io_to_errno(e),
    }
}

/// Initializes a runtime-scoped external packet tunnel using the default Swift -> Leaf
/// input queue size.
///
/// Use `leaf_packet_tunnel_init_with_options` when you need to tune ingress backpressure.
#[no_mangle]
pub extern "C" fn leaf_packet_tunnel_init(rt_id: u16, output_queue_size: usize) -> i32 {
    leaf_packet_tunnel_init_with_options(rt_id, 0, output_queue_size)
}

/// Registers or clears the callback that notifies Swift when Leaf has ingress capacity again.
///
/// The callback is edge-triggered. It fires when the Swift -> Leaf input queue transitions
/// from full to non-full after `leaf_packet_tunnel_write` has returned `ERR_QUEUE_FULL`.
/// It is not a close signal. After `leaf_packet_tunnel_close` returns, the callback will
/// no longer be invoked.
/// The callback must return quickly and should schedule a retry of any locally buffered packets.
///
/// @param rt_id The runtime ID associated with this packet tunnel.
/// @param context User context pointer passed back to the callback.
/// @param callback Nullable callback invoked as `callback(rt_id, context)`.
/// @return Returns ERR_OK on success.
#[no_mangle]
pub extern "C" fn leaf_packet_tunnel_set_input_ready_callback(
    rt_id: u16,
    context: *mut c_void,
    callback: Option<extern "C" fn(u16, *mut c_void)>,
) -> i32 {
    let notifier = callback.map(|callback| {
        leaf::proxy::tun::packet_io::PacketTunnelNotifier::new(rt_id, context, callback)
    });
    match leaf::proxy::tun::packet_io::set_runtime_packet_input_callback(rt_id, notifier) {
        Ok(()) => ERR_OK,
        Err(e) => io_to_errno(e),
    }
}

/// Registers or clears the callback that notifies Swift when Leaf has outbound packets ready.
///
/// The callback is invoked when the outbound packet queue transitions from empty to non-empty.
/// If packets are already queued when the callback is registered, the callback is invoked
/// immediately so Swift can start draining without waiting for a new edge.
/// After `leaf_packet_tunnel_close` returns, the callback will no longer be invoked.
/// The callback must return quickly and should schedule a drain loop that repeatedly calls
/// `leaf_packet_tunnel_read` until it returns ERR_NO_DATA.
///
/// @param rt_id The runtime ID associated with this packet tunnel.
/// @param context User context pointer passed back to the callback.
/// @param callback Nullable callback invoked as `callback(rt_id, context)`.
/// @return Returns ERR_OK on success.
#[no_mangle]
pub extern "C" fn leaf_packet_tunnel_set_output_callback(
    rt_id: u16,
    context: *mut c_void,
    callback: Option<extern "C" fn(u16, *mut c_void)>,
) -> i32 {
    let notifier = callback.map(|callback| {
        leaf::proxy::tun::packet_io::PacketTunnelNotifier::new(rt_id, context, callback)
    });
    match leaf::proxy::tun::packet_io::set_runtime_packet_output_callback(rt_id, notifier) {
        Ok(()) => ERR_OK,
        Err(e) => io_to_errno(e),
    }
}

/// Writes one raw IP packet from Swift into Leaf.
///
/// This is the ingress path for packets read from `NEPacketTunnelFlow.readPackets`.
///
/// @param rt_id The runtime ID associated with this packet tunnel.
/// @param packet Pointer to the raw IP packet bytes.
/// @param packet_len Packet length in bytes.
/// @return Returns ERR_OK on success, ERR_QUEUE_FULL if the bounded ingress queue is saturated,
///         ERR_IO if the tunnel has already been closed.
#[no_mangle]
pub unsafe extern "C" fn leaf_packet_tunnel_write(
    rt_id: u16,
    packet: *const u8,
    packet_len: usize,
) -> i32 {
    if packet_len == 0 {
        return ERR_OK;
    }
    if packet.is_null() {
        return ERR_INVALID_ARGUMENT;
    }
    let packet = unsafe { slice::from_raw_parts(packet, packet_len) };
    match leaf::proxy::tun::packet_io::write_runtime_packet(rt_id, packet) {
        Ok(()) => ERR_OK,
        Err(e) => io_to_errno(e),
    }
}

/// Reads one raw IP packet produced by Leaf.
///
/// This is the egress path that Swift should write back into `NEPacketTunnelFlow.writePackets`.
///
/// @param rt_id The runtime ID associated with this packet tunnel.
/// @param buffer Destination buffer for the packet bytes.
/// @param buffer_len Size of `buffer` in bytes. A 65535-byte scratch buffer is sufficient.
///                   Passing `buffer = NULL` with `buffer_len = 0` is allowed and can be used
///                   to query the required packet size without consuming the packet.
/// @param packet_len Output pointer receiving the actual packet size. When the function returns
///                   ERR_BUFFER_TOO_SMALL, this still receives the required size and the packet
///                   remains queued for the next read attempt.
/// @return Returns ERR_OK when a packet was copied, ERR_NO_DATA when no packet is available.
#[no_mangle]
pub unsafe extern "C" fn leaf_packet_tunnel_read(
    rt_id: u16,
    buffer: *mut u8,
    buffer_len: usize,
    packet_len: *mut usize,
) -> i32 {
    if packet_len.is_null() {
        return ERR_INVALID_ARGUMENT;
    }

    if buffer.is_null() && buffer_len > 0 {
        return ERR_INVALID_ARGUMENT;
    }

    let buffer = if buffer_len == 0 {
        &mut []
    } else {
        unsafe { slice::from_raw_parts_mut(buffer, buffer_len) }
    };
    match leaf::proxy::tun::packet_io::read_runtime_packet(rt_id, buffer) {
        Ok(leaf::proxy::tun::packet_io::RuntimePacketRead::Packet(len)) => {
            unsafe { *packet_len = len };
            ERR_OK
        }
        Ok(leaf::proxy::tun::packet_io::RuntimePacketRead::NoData) => {
            unsafe { *packet_len = 0 };
            ERR_NO_DATA
        }
        Ok(leaf::proxy::tun::packet_io::RuntimePacketRead::BufferTooSmall(len)) => {
            unsafe { *packet_len = len };
            ERR_BUFFER_TOO_SMALL
        }
        Err(e) => io_to_errno(e),
    }
}

/// Closes and removes the external packet tunnel associated with the runtime.
///
/// This function is idempotent and must be called before reinitializing the same `rt_id`.
#[no_mangle]
pub extern "C" fn leaf_packet_tunnel_close(rt_id: u16) -> i32 {
    leaf::proxy::tun::packet_io::remove_runtime_packet_tunnel(rt_id);
    ERR_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    extern "C" fn noop_test_outbounds_callback(
        _tag: *const c_char,
        _tcp_latency: i32,
        _udp_latency: i32,
        _context: *mut c_void,
    ) {
    }

    #[test]
    fn packet_tunnel_write_reports_queue_full() {
        let rt_id = 51_001;
        let packet = [1_u8, 2, 3];

        assert_eq!(leaf_packet_tunnel_init_with_options(rt_id, 1, 1), ERR_OK);
        assert_eq!(
            unsafe { leaf_packet_tunnel_write(rt_id, packet.as_ptr(), packet.len()) },
            ERR_OK
        );
        assert_eq!(
            unsafe { leaf_packet_tunnel_write(rt_id, packet.as_ptr(), packet.len()) },
            ERR_QUEUE_FULL
        );

        assert_eq!(leaf_packet_tunnel_close(rt_id), ERR_OK);
    }

    #[test]
    fn packet_tunnel_init_rejects_queue_sizes_above_maximum() {
        assert_eq!(
            leaf_packet_tunnel_init_with_options(51_002, MAX_PACKET_TUNNEL_QUEUE_SIZE + 1, 1),
            ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn outbound_tag_utf8_errors_return_invalid_argument() {
        let invalid = [0xff_u8, 0x00];
        let tag = invalid.as_ptr().cast::<c_char>();
        let mut ts = 0_u32;

        assert_eq!(
            unsafe { leaf_health_check(1, tag, 0) },
            ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            unsafe { leaf_get_last_active(1, tag, &mut ts as *mut u32) },
            ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            unsafe { leaf_get_since_last_active(1, tag, &mut ts as *mut u32) },
            ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn null_config_arguments_preserve_config_path_error() {
        assert_eq!(unsafe { leaf_run(1, ptr::null()) }, ERR_CONFIG_PATH);
        let mut ts = 0_u32;

        assert_eq!(
            unsafe { leaf_run_with_config_string(1, ptr::null()) },
            ERR_CONFIG_PATH
        );
        assert_eq!(
            unsafe { leaf_run_with_options(1, ptr::null(), false, false, false, 0, 0) },
            ERR_CONFIG_PATH
        );
        assert_eq!(unsafe { leaf_test_config(ptr::null()) }, ERR_CONFIG_PATH);
        assert_eq!(
            unsafe {
                leaf_test_outbounds(
                    ptr::null(),
                    1,
                    1,
                    ptr::null_mut(),
                    noop_test_outbounds_callback,
                )
            },
            ERR_CONFIG_PATH
        );
        assert_eq!(
            unsafe { leaf_health_check(1, ptr::null(), 0) },
            ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            unsafe { leaf_get_last_active(1, ptr::null(), &mut ts as *mut u32) },
            ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            unsafe { leaf_get_since_last_active(1, ptr::null(), &mut ts as *mut u32) },
            ERR_INVALID_ARGUMENT
        );
    }

    #[test]
    fn ffi_boundary_converts_panics_to_err_io() {
        assert_eq!(ffi_boundary("test", || panic!("boom")), ERR_IO);
    }

    #[test]
    fn callback_cstring_replaces_interior_nuls() {
        let value = callback_cstring("ab\0cd").expect("sanitized cstring");
        assert_eq!(value.to_str().unwrap(), "ab\u{FFFD}cd");
    }
}

/// Tests the configuration.
///
/// @param config_path The path of the config file, must be a file with suffix .conf
///                    or .json, according to the enabled features.
/// @return Returns ERR_OK on success, i.e no syntax error.
#[no_mangle]
pub unsafe extern "C" fn leaf_test_config(config_path: *const c_char) -> i32 {
    let config_path =
        match unsafe { parse_string_arg(config_path, ERR_CONFIG_PATH, ERR_CONFIG_PATH) } {
            Ok(config_path) => config_path,
            Err(err) => return err,
        };
    if let Err(e) = leaf::test_config(&config_path) {
        return to_errno(e);
    }
    ERR_OK
}

/// Tests all outbounds connectivity and latency.
///
/// @param config The content of the config file.
/// @param concurrency The maximum number of concurrent tests.
/// @param timeout_sec The timeout in seconds for each test.
/// @param context User-provided context pointer to be passed back to the callback.
/// @param callback The callback function to receive results.
///                 Arguments: tag (string), tcp_latency (ms, -1 if failed), udp_latency (ms, -1 if failed), context.
/// @return Returns ERR_OK on success.
#[no_mangle]
pub unsafe extern "C" fn leaf_test_outbounds(
    config: *const c_char,
    concurrency: u32,
    timeout_sec: u32,
    context: *mut std::ffi::c_void,
    callback: extern "C" fn(*const c_char, i32, i32, *mut std::ffi::c_void),
) -> i32 {
    ffi_boundary("leaf_test_outbounds", || {
        let config_str = match unsafe { parse_string_arg(config, ERR_CONFIG_PATH, ERR_CONFIG_PATH) }
        {
            Ok(config_str) => config_str,
            Err(err) => return err,
        };
        let config = match leaf::config::from_string(&config_str) {
            Ok(c) => c,
            Err(e) => return to_errno(leaf::Error::Config(anyhow::anyhow!(e))),
        };

        match block_on_current_thread(async move {
            use futures::StreamExt;
            let timeout = if timeout_sec > 0 {
                Some(std::time::Duration::from_secs(timeout_sec as u64))
            } else {
                None
            };
            if let Ok(mut stream) =
                leaf::util::stream_outbounds_tests(&config, timeout, concurrency as usize).await
            {
                while let Some((tag, (tcp_res, udp_res))) = stream.next().await {
                    let Some(tag_cstring) = callback_cstring(&tag) else {
                        tracing::error!("failed to encode outbound tag for callback");
                        continue;
                    };
                    let tcp_latency = match tcp_res {
                        Ok(d) => d.as_millis() as i32,
                        Err(e) => {
                            tracing::warn!("TCP test failed for {}: {:?}", tag, e);
                            -1
                        }
                    };
                    let udp_latency = match udp_res {
                        Ok(d) => d.as_millis() as i32,
                        Err(e) => {
                            tracing::warn!("UDP test failed for {}: {:?}", tag, e);
                            -1
                        }
                    };
                    callback(tag_cstring.as_ptr(), tcp_latency, udp_latency, context);
                }
            } else {
                tracing::warn!("Failed to start stream_outbounds_tests");
            }
        }) {
            Ok(()) => ERR_OK,
            Err(err) => err,
        }
    })
}

/// Runs a health check for an outbound.
///
/// This performs an active health check by sending a PING to healthcheck.leaf
/// and waiting for a PONG response through the specified outbound, testing both
/// TCP and UDP protocols.
///
/// @param rt_id The ID of the leaf instance.
/// @param outbound_tag The tag of the outbound to test.
/// @param timeout_ms Timeout in milliseconds (0 for default 4 seconds).
/// @return Returns ERR_OK if either TCP or UDP health check succeeds, error code otherwise.
#[no_mangle]
pub unsafe extern "C" fn leaf_health_check(
    rt_id: u16,
    outbound_tag: *const c_char,
    timeout_ms: u64,
) -> i32 {
    ffi_boundary("leaf_health_check", || {
        use std::time::Duration;

        let outbound_tag = match unsafe {
            parse_string_arg(outbound_tag, ERR_INVALID_ARGUMENT, ERR_INVALID_ARGUMENT)
        } {
            Ok(tag) => tag,
            Err(err) => return err,
        };

        let manager = leaf::RUNTIME_MANAGER.lock().unwrap().get(&rt_id).cloned();
        let result = if let Some(m) = manager {
            let timeout = if timeout_ms == 0 {
                None
            } else {
                Some(Duration::from_millis(timeout_ms))
            };
            match block_on_current_thread(async move {
                m.health_check_outbound(&outbound_tag, timeout).await
            }) {
                Ok(result) => result,
                Err(err) => return err,
            }
        } else {
            Err(leaf::Error::RuntimeManager)
        };

        match result {
            Ok((tcp_res, udp_res)) => {
                if tcp_res.is_ok() || udp_res.is_ok() {
                    ERR_OK
                } else {
                    ERR_IO
                }
            }
            Err(e) => to_errno(e),
        }
    })
}

/// Gets the last peer-data timestamp for an outbound.
///
/// Scans currently-active sessions for the outbound and returns the most recent
/// timestamp at which inbound data was received on any of them. This is NOT a
/// historical value: completed sessions are not included. If no sessions for the
/// outbound are currently open, ERR_NO_DATA is returned regardless of how recently
/// traffic flowed through sessions that have since closed.
///
/// @param rt_id The ID of the leaf instance.
/// @param outbound_tag The tag of the outbound.
/// @param timestamp_s Pointer to store the timestamp in seconds since Unix epoch.
/// @return Returns ERR_OK on success, ERR_NO_DATA if no active sessions exist for
///         the outbound, error code otherwise.
#[no_mangle]
pub unsafe extern "C" fn leaf_get_last_active(
    rt_id: u16,
    outbound_tag: *const c_char,
    timestamp_s: *mut u32,
) -> i32 {
    ffi_boundary("leaf_get_last_active", || {
        if timestamp_s.is_null() {
            return ERR_INVALID_ARGUMENT;
        }
        let outbound_tag = match unsafe {
            parse_string_arg(outbound_tag, ERR_INVALID_ARGUMENT, ERR_INVALID_ARGUMENT)
        } {
            Ok(tag) => tag,
            Err(err) => return err,
        };

        let manager = leaf::RUNTIME_MANAGER.lock().unwrap().get(&rt_id).cloned();
        let result = if let Some(m) = manager {
            match block_on_current_thread(async move {
                m.get_outbound_last_peer_active(&outbound_tag).await
            }) {
                Ok(result) => result,
                Err(err) => return err,
            }
        } else {
            return to_errno(leaf::Error::RuntimeManager);
        };

        match result {
            Ok(Some(ts)) => {
                unsafe { *timestamp_s = ts };
                ERR_OK
            }
            Ok(None) => ERR_NO_DATA,
            Err(e) => to_errno(e),
        }
    })
}

/// Gets the elapsed seconds since the last peer-data receipt for an outbound.
///
/// Equivalent to (now - leaf_get_last_active), but computed atomically in Rust.
/// Like leaf_get_last_active, this only considers currently-active sessions;
/// sessions that have already closed are not reflected. ERR_NO_DATA is returned
/// when no sessions for the outbound are currently open.
///
/// @param rt_id The ID of the leaf instance.
/// @param outbound_tag The tag of the outbound.
/// @param since_s Pointer to store the elapsed seconds since last peer data receipt.
/// @return Returns ERR_OK on success, ERR_NO_DATA if no active sessions exist for
///         the outbound, error code otherwise.
#[no_mangle]
pub unsafe extern "C" fn leaf_get_since_last_active(
    rt_id: u16,
    outbound_tag: *const c_char,
    since_s: *mut u32,
) -> i32 {
    ffi_boundary("leaf_get_since_last_active", || {
        if since_s.is_null() {
            return ERR_INVALID_ARGUMENT;
        }
        let outbound_tag = match unsafe {
            parse_string_arg(outbound_tag, ERR_INVALID_ARGUMENT, ERR_INVALID_ARGUMENT)
        } {
            Ok(tag) => tag,
            Err(err) => return err,
        };

        let manager = leaf::RUNTIME_MANAGER.lock().unwrap().get(&rt_id).cloned();
        let result = if let Some(m) = manager {
            match block_on_current_thread(async move {
                m.get_outbound_last_peer_active(&outbound_tag).await
            }) {
                Ok(result) => result,
                Err(err) => return err,
            }
        } else {
            return to_errno(leaf::Error::RuntimeManager);
        };

        match result {
            Ok(Some(ts)) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as u32)
                    .unwrap_or(0);
                let since = now.saturating_sub(ts);
                unsafe { *since_s = since };
                ERR_OK
            }
            Ok(None) => ERR_NO_DATA,
            Err(e) => to_errno(e),
        }
    })
}
