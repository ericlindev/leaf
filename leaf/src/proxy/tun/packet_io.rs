use std::{
    collections::{HashMap, VecDeque},
    ffi::c_void,
    io::{self, ErrorKind},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    task::{Context, Poll},
};

use futures::{task::AtomicWaker, Sink, Stream};
use lazy_static::lazy_static;

use crate::{option, RuntimeId};

type PacketQueue = Mutex<VecDeque<Vec<u8>>>;
pub const MAX_PACKET_TUNNEL_QUEUE_SIZE: usize = 4_096;
pub const DEFAULT_INPUT_BYTE_LIMIT: usize = 1 * 1024 * 1024; // 1 MB
pub const DEFAULT_OUTPUT_BYTE_LIMIT: usize = 2 * 1024 * 1024; // 2 MB
const MAX_IP_PACKET_SIZE: usize = u16::MAX as usize;

struct OutputState {
    queue: VecDeque<Vec<u8>>,
    pending: Option<Vec<u8>>,
}

#[derive(Clone, Copy)]
pub struct PacketTunnelNotifier {
    rt_id: RuntimeId,
    context: *mut c_void,
    callback: extern "C" fn(RuntimeId, *mut c_void),
}

unsafe impl Send for PacketTunnelNotifier {}
unsafe impl Sync for PacketTunnelNotifier {}

impl PacketTunnelNotifier {
    pub fn new(
        rt_id: RuntimeId,
        context: *mut c_void,
        callback: extern "C" fn(RuntimeId, *mut c_void),
    ) -> Self {
        Self {
            rt_id,
            context,
            callback,
        }
    }

    fn notify(self) {
        (self.callback)(self.rt_id, self.context);
    }
}

pub struct PacketTunnelTransport {
    inner: Arc<PacketTunnelHandle>,
}

pub enum RuntimePacketRead {
    NoData,
    BufferTooSmall(usize),
    Packet(usize),
}

impl Stream for PacketTunnelTransport {
    type Item = io::Result<Vec<u8>>;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(packet) = self.inner.pop_input_packet() {
            return Poll::Ready(Some(Ok(packet)));
        }
        if self.inner.closed.load(Ordering::Acquire) {
            return Poll::Ready(None);
        }

        self.inner.input_waker.register(cx.waker());

        if let Some(packet) = self.inner.pop_input_packet() {
            return Poll::Ready(Some(Ok(packet)));
        }
        if self.inner.closed.load(Ordering::Acquire) {
            return Poll::Ready(None);
        }
        Poll::Pending
    }
}

impl Sink<Vec<u8>> for PacketTunnelTransport {
    type Error = io::Error;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Poll::Ready(Err(io::Error::new(
                ErrorKind::BrokenPipe,
                "packet tunnel is closed",
            )));
        }
        if self.inner.output_has_capacity() {
            return Poll::Ready(Ok(()));
        }

        self.inner.output_waker.register(cx.waker());
        if self.inner.output_has_capacity() {
            return Poll::Ready(Ok(()));
        }
        if self.inner.closed.load(Ordering::Acquire) {
            return Poll::Ready(Err(io::Error::new(
                ErrorKind::BrokenPipe,
                "packet tunnel is closed",
            )));
        }
        Poll::Pending
    }

    fn start_send(self: std::pin::Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
        if item.is_empty() {
            return Ok(());
        }
        self.inner.push_output_packet(item)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.inner.close();
        Poll::Ready(Ok(()))
    }
}

struct PacketTunnelHandle {
    input_queue: PacketQueue,
    input_waker: AtomicWaker,
    input_ready_notifier: Mutex<Option<PacketTunnelNotifier>>,
    input_queue_limit: usize,
    input_queued_bytes: AtomicUsize,
    input_byte_limit: usize,
    output_state: Mutex<OutputState>,
    output_waker: AtomicWaker,
    output_ready_notifier: Mutex<Option<PacketTunnelNotifier>>,
    output_queue_limit: usize,
    output_queued_bytes: AtomicUsize,
    output_byte_limit: usize,
    callbacks_in_flight: Mutex<usize>,
    callbacks_drained: Condvar,
    connected: AtomicBool,
    closed: AtomicBool,
}

struct CallbackGuard<'a> {
    handle: &'a PacketTunnelHandle,
}

impl Drop for CallbackGuard<'_> {
    fn drop(&mut self) {
        self.handle.finish_callback();
    }
}

impl PacketTunnelHandle {
    fn new(
        input_queue_limit: usize,
        output_queue_limit: usize,
        input_byte_limit: usize,
        output_byte_limit: usize,
    ) -> Self {
        Self {
            input_queue: Mutex::new(VecDeque::new()),
            input_waker: AtomicWaker::new(),
            input_ready_notifier: Mutex::new(None),
            input_queue_limit,
            input_queued_bytes: AtomicUsize::new(0),
            input_byte_limit,
            output_state: Mutex::new(OutputState {
                queue: VecDeque::new(),
                pending: None,
            }),
            output_waker: AtomicWaker::new(),
            output_ready_notifier: Mutex::new(None),
            output_queue_limit,
            output_queued_bytes: AtomicUsize::new(0),
            output_byte_limit: output_byte_limit.max(MAX_IP_PACKET_SIZE),
            callbacks_in_flight: Mutex::new(0),
            callbacks_drained: Condvar::new(),
            connected: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    fn take_transport(self: &Arc<Self>) -> io::Result<PacketTunnelTransport> {
        if self.connected.swap(true, Ordering::AcqRel) {
            return Err(io::Error::new(
                ErrorKind::AlreadyExists,
                "packet tunnel is already connected",
            ));
        }
        Ok(PacketTunnelTransport {
            inner: self.clone(),
        })
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        *self.input_ready_notifier.lock().unwrap() = None;
        *self.output_ready_notifier.lock().unwrap() = None;
        self.wait_for_callbacks();
        self.input_waker.wake();
        self.output_waker.wake();
    }

    fn begin_callback(&self) -> Option<CallbackGuard<'_>> {
        let mut callbacks_in_flight = self.callbacks_in_flight.lock().unwrap();
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        *callbacks_in_flight += 1;
        Some(CallbackGuard { handle: self })
    }

    fn finish_callback(&self) {
        let mut callbacks_in_flight = self.callbacks_in_flight.lock().unwrap();
        *callbacks_in_flight -= 1;
        if *callbacks_in_flight == 0 {
            self.callbacks_drained.notify_all();
        }
    }

    fn wait_for_callbacks(&self) {
        let mut callbacks_in_flight = self.callbacks_in_flight.lock().unwrap();
        while *callbacks_in_flight > 0 {
            callbacks_in_flight = self.callbacks_drained.wait(callbacks_in_flight).unwrap();
        }
    }

    fn pop_input_packet(&self) -> Option<Vec<u8>> {
        let (packet, should_notify) = {
            let mut queue = self.input_queue.lock().unwrap();
            let was_count_full = queue.len() >= self.input_queue_limit;
            let was_byte_full =
                self.input_queued_bytes.load(Ordering::Relaxed) >= self.input_byte_limit;
            let was_full = was_count_full || was_byte_full;
            let packet = queue.pop_front();
            if let Some(ref p) = packet {
                self.input_queued_bytes
                    .fetch_sub(p.len(), Ordering::Relaxed);
            }
            let now_count_full = queue.len() >= self.input_queue_limit;
            let now_byte_full =
                self.input_queued_bytes.load(Ordering::Relaxed) >= self.input_byte_limit;
            let should_notify = packet.is_some() && was_full && !(now_count_full || now_byte_full);
            (packet, should_notify)
        };

        if should_notify {
            if let Some(_guard) = self.begin_callback() {
                let notifier = *self.input_ready_notifier.lock().unwrap();
                if let Some(notifier) = notifier {
                    notifier.notify();
                }
            }
        }

        packet
    }

    fn push_input_packet(&self, packet: Vec<u8>) -> io::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                ErrorKind::BrokenPipe,
                "packet tunnel is closed",
            ));
        }

        {
            let mut queue = self.input_queue.lock().unwrap();
            if queue.len() >= self.input_queue_limit {
                return Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "packet tunnel input queue is full",
                ));
            }
            if self.input_queued_bytes.load(Ordering::Relaxed) + packet.len()
                > self.input_byte_limit
            {
                return Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "packet tunnel input byte limit exceeded",
                ));
            }
            self.input_queued_bytes
                .fetch_add(packet.len(), Ordering::Relaxed);
            queue.push_back(packet);
        }

        self.input_waker.wake();
        Ok(())
    }

    fn set_input_ready_notifier(&self, notifier: Option<PacketTunnelNotifier>) {
        *self.input_ready_notifier.lock().unwrap() = notifier;
    }

    fn output_has_capacity(&self) -> bool {
        let state = self.output_state.lock().unwrap();
        let count_ok =
            state.queue.len() + usize::from(state.pending.is_some()) < self.output_queue_limit;
        // Reserve space for one full IP packet so `poll_ready` guarantees the next
        // `start_send` cannot fail with `WouldBlock` due to byte pressure alone.
        let queued_bytes = self.output_queued_bytes.load(Ordering::Relaxed);
        let byte_ok = self.output_byte_limit.saturating_sub(queued_bytes) >= MAX_IP_PACKET_SIZE;
        count_ok && byte_ok
    }

    fn push_output_packet(&self, item: Vec<u8>) -> io::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                ErrorKind::BrokenPipe,
                "packet tunnel is closed",
            ));
        }

        let should_notify = {
            let mut state = self.output_state.lock().unwrap();
            if state.queue.len() + usize::from(state.pending.is_some()) >= self.output_queue_limit {
                return Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "packet tunnel output queue is full",
                ));
            }
            if self.output_queued_bytes.load(Ordering::Relaxed) + item.len()
                > self.output_byte_limit
            {
                return Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "packet tunnel output byte limit exceeded",
                ));
            }
            self.output_queued_bytes
                .fetch_add(item.len(), Ordering::Relaxed);
            let should_notify = state.queue.is_empty() && state.pending.is_none();
            state.queue.push_back(item);
            should_notify
        };

        if should_notify {
            if let Some(_guard) = self.begin_callback() {
                let notifier = *self.output_ready_notifier.lock().unwrap();
                if let Some(notifier) = notifier {
                    notifier.notify();
                }
            }
        }
        Ok(())
    }

    fn set_output_ready_notifier(&self, notifier: Option<PacketTunnelNotifier>) {
        *self.output_ready_notifier.lock().unwrap() = notifier;
        if let Some(notifier) = notifier.filter(|_| self.has_output_ready()) {
            if let Some(_guard) = self.begin_callback() {
                notifier.notify();
            }
        }
    }

    fn has_output_ready(&self) -> bool {
        let state = self.output_state.lock().unwrap();
        state.pending.is_some() || !state.queue.is_empty()
    }

    fn read_output_packet(&self, buffer: &mut [u8]) -> io::Result<RuntimePacketRead> {
        let mut state = self.output_state.lock().unwrap();
        if state.pending.is_none() {
            state.pending = state.queue.pop_front();
        }
        let Some(packet) = state.pending.as_ref() else {
            return Ok(RuntimePacketRead::NoData);
        };
        if buffer.len() < packet.len() {
            return Ok(RuntimePacketRead::BufferTooSmall(packet.len()));
        }
        let len = packet.len();
        buffer[..len].copy_from_slice(packet);
        state.pending.take();
        drop(state);
        // Decrement only after the packet is fully consumed (copied to the caller's buffer).
        self.output_queued_bytes.fetch_sub(len, Ordering::Relaxed);
        self.output_waker.wake();
        Ok(RuntimePacketRead::Packet(len))
    }
}

lazy_static! {
    static ref PACKET_TUNNELS: Mutex<HashMap<RuntimeId, Arc<PacketTunnelHandle>>> =
        Mutex::new(HashMap::new());
}

fn packet_queue_size(queue_size: usize) -> usize {
    queue_size.max(1)
}

fn normalize_packet_queue_size(queue_size: usize, default_size: usize) -> io::Result<usize> {
    if queue_size == 0 {
        return Ok(packet_queue_size(default_size).min(MAX_PACKET_TUNNEL_QUEUE_SIZE));
    }

    let queue_size = packet_queue_size(queue_size);
    if queue_size > MAX_PACKET_TUNNEL_QUEUE_SIZE {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "packet tunnel queue size {} exceeds maximum {}",
                queue_size, MAX_PACKET_TUNNEL_QUEUE_SIZE
            ),
        ));
    }
    Ok(queue_size)
}

fn get_handle(rt_id: RuntimeId) -> io::Result<Arc<PacketTunnelHandle>> {
    PACKET_TUNNELS
        .lock()
        .unwrap()
        .get(&rt_id)
        .cloned()
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "packet tunnel not initialized"))
}

pub fn init_runtime_packet_tunnel(
    rt_id: RuntimeId,
    input_queue_size: usize,
    output_queue_size: usize,
) -> io::Result<()> {
    if crate::is_running(rt_id) {
        return Err(io::Error::new(
            ErrorKind::AlreadyExists,
            "runtime is already running",
        ));
    }

    let input_queue_size =
        normalize_packet_queue_size(input_queue_size, *option::PACKET_TUNNEL_INPUT_QUEUE_SIZE)?;
    let output_queue_size =
        normalize_packet_queue_size(output_queue_size, *option::NETSTACK_OUTPUT_CHANNEL_SIZE)?;

    let mut tunnels = PACKET_TUNNELS.lock().unwrap();
    if tunnels.contains_key(&rt_id) {
        return Err(io::Error::new(
            ErrorKind::AlreadyExists,
            "packet tunnel is already initialized",
        ));
    }

    let handle = Arc::new(PacketTunnelHandle::new(
        input_queue_size,
        output_queue_size,
        DEFAULT_INPUT_BYTE_LIMIT,
        DEFAULT_OUTPUT_BYTE_LIMIT,
    ));
    tunnels.insert(rt_id, handle);
    Ok(())
}

pub fn has_runtime_packet_tunnel(rt_id: RuntimeId) -> bool {
    PACKET_TUNNELS.lock().unwrap().contains_key(&rt_id)
}

pub fn take_runtime_packet_tunnel(rt_id: RuntimeId) -> io::Result<Option<PacketTunnelTransport>> {
    let handle = PACKET_TUNNELS.lock().unwrap().get(&rt_id).cloned();
    match handle {
        Some(handle) => handle.take_transport().map(Some),
        None => Ok(None),
    }
}

pub fn remove_runtime_packet_tunnel(rt_id: RuntimeId) {
    if let Some(handle) = PACKET_TUNNELS.lock().unwrap().remove(&rt_id) {
        handle.close();
    }
}

pub fn set_runtime_packet_input_callback(
    rt_id: RuntimeId,
    notifier: Option<PacketTunnelNotifier>,
) -> io::Result<()> {
    get_handle(rt_id)?.set_input_ready_notifier(notifier);
    Ok(())
}

pub fn set_runtime_packet_output_callback(
    rt_id: RuntimeId,
    notifier: Option<PacketTunnelNotifier>,
) -> io::Result<()> {
    get_handle(rt_id)?.set_output_ready_notifier(notifier);
    Ok(())
}

pub fn write_runtime_packet(rt_id: RuntimeId, packet: &[u8]) -> io::Result<()> {
    if packet.is_empty() {
        return Ok(());
    }
    get_handle(rt_id)?.push_input_packet(packet.to_vec())
}

pub fn read_runtime_packet(rt_id: RuntimeId, buffer: &mut [u8]) -> io::Result<RuntimePacketRead> {
    get_handle(rt_id)?.read_output_packet(buffer)
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::c_void,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use futures::{SinkExt, StreamExt};

    use super::*;

    extern "C" fn count_notifications(_rt_id: RuntimeId, context: *mut c_void) {
        let counter = unsafe { &*(context as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn packet_tunnel_roundtrip() {
        let rt_id = 50_001;
        init_runtime_packet_tunnel(rt_id, 2, 2).unwrap();
        let mut tunnel = take_runtime_packet_tunnel(rt_id).unwrap().unwrap();

        write_runtime_packet(rt_id, &[1, 2, 3]).unwrap();
        let inbound = futures::executor::block_on(async { tunnel.next().await.unwrap().unwrap() });
        assert_eq!(inbound, vec![1, 2, 3]);

        futures::executor::block_on(async { tunnel.send(vec![4, 5, 6]).await }).unwrap();
        let mut buffer = [0u8; 8];
        let len = match read_runtime_packet(rt_id, &mut buffer).unwrap() {
            RuntimePacketRead::Packet(len) => len,
            _ => panic!("expected packet"),
        };
        assert_eq!(&buffer[..len], &[4, 5, 6]);

        remove_runtime_packet_tunnel(rt_id);
    }

    #[test]
    fn output_notification_fires_on_empty_to_non_empty_transition() {
        let rt_id = 50_002;
        let counter = Box::new(AtomicUsize::new(0));
        let counter_ptr = Box::into_raw(counter);

        init_runtime_packet_tunnel(rt_id, 4, 4).unwrap();
        set_runtime_packet_output_callback(
            rt_id,
            Some(PacketTunnelNotifier {
                rt_id,
                context: counter_ptr.cast(),
                callback: count_notifications,
            }),
        )
        .unwrap();
        let mut tunnel = take_runtime_packet_tunnel(rt_id).unwrap().unwrap();

        futures::executor::block_on(async {
            tunnel.send(vec![1]).await.unwrap();
            tunnel.send(vec![2]).await.unwrap();
        });

        let counter = unsafe { Box::from_raw(counter_ptr) };
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        drop(counter);

        remove_runtime_packet_tunnel(rt_id);
    }

    #[test]
    fn read_keeps_packet_pending_when_buffer_is_too_small() {
        let rt_id = 50_003;
        init_runtime_packet_tunnel(rt_id, 2, 2).unwrap();
        let mut tunnel = take_runtime_packet_tunnel(rt_id).unwrap().unwrap();

        futures::executor::block_on(async { tunnel.send(vec![9, 8, 7]).await }).unwrap();

        let mut short = [0u8; 2];
        let len = match read_runtime_packet(rt_id, &mut short).unwrap() {
            RuntimePacketRead::BufferTooSmall(len) => len,
            _ => panic!("expected buffer-too-small"),
        };
        assert_eq!(len, 3);

        let mut full = [0u8; 8];
        let len = match read_runtime_packet(rt_id, &mut full).unwrap() {
            RuntimePacketRead::Packet(len) => len,
            _ => panic!("expected packet"),
        };
        assert_eq!(&full[..len], &[9, 8, 7]);

        remove_runtime_packet_tunnel(rt_id);
    }

    #[test]
    fn write_returns_would_block_when_input_queue_is_full() {
        let rt_id = 50_004;
        init_runtime_packet_tunnel(rt_id, 1, 2).unwrap();

        write_runtime_packet(rt_id, &[1, 2, 3]).unwrap();
        let err = write_runtime_packet(rt_id, &[4, 5, 6]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::WouldBlock);

        remove_runtime_packet_tunnel(rt_id);
    }

    #[test]
    fn input_notification_fires_on_full_to_non_full_transition() {
        let rt_id = 50_005;
        let counter = Box::new(AtomicUsize::new(0));
        let counter_ptr = Box::into_raw(counter);

        init_runtime_packet_tunnel(rt_id, 1, 2).unwrap();
        set_runtime_packet_input_callback(
            rt_id,
            Some(PacketTunnelNotifier {
                rt_id,
                context: counter_ptr.cast(),
                callback: count_notifications,
            }),
        )
        .unwrap();

        write_runtime_packet(rt_id, &[1]).unwrap();
        let mut tunnel = take_runtime_packet_tunnel(rt_id).unwrap().unwrap();
        let inbound = futures::executor::block_on(async { tunnel.next().await.unwrap().unwrap() });
        assert_eq!(inbound, vec![1]);

        let counter = unsafe { Box::from_raw(counter_ptr) };
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        drop(counter);

        remove_runtime_packet_tunnel(rt_id);
    }

    #[test]
    fn init_rejects_reinitialization_until_closed() {
        let rt_id = 50_006;
        init_runtime_packet_tunnel(rt_id, 1, 1).unwrap();

        let err = init_runtime_packet_tunnel(rt_id, 1, 1).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);

        remove_runtime_packet_tunnel(rt_id);
        init_runtime_packet_tunnel(rt_id, 1, 1).unwrap();
        remove_runtime_packet_tunnel(rt_id);
    }

    #[test]
    fn init_rejects_queue_sizes_above_maximum() {
        let rt_id = 50_007;
        let err =
            init_runtime_packet_tunnel(rt_id, MAX_PACKET_TUNNEL_QUEUE_SIZE + 1, 1).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn zero_queue_size_clamps_large_default() {
        let queue_size = normalize_packet_queue_size(0, MAX_PACKET_TUNNEL_QUEUE_SIZE + 1).unwrap();
        assert_eq!(queue_size, MAX_PACKET_TUNNEL_QUEUE_SIZE);
    }

    #[test]
    fn input_notification_does_not_fire_after_close() {
        let rt_id = 50_008;
        let counter = Box::new(AtomicUsize::new(0));
        let counter_ptr = Box::into_raw(counter);

        init_runtime_packet_tunnel(rt_id, 1, 1).unwrap();
        set_runtime_packet_input_callback(
            rt_id,
            Some(PacketTunnelNotifier {
                rt_id,
                context: counter_ptr.cast(),
                callback: count_notifications,
            }),
        )
        .unwrap();

        write_runtime_packet(rt_id, &[1]).unwrap();
        let mut tunnel = take_runtime_packet_tunnel(rt_id).unwrap().unwrap();
        remove_runtime_packet_tunnel(rt_id);
        let inbound = futures::executor::block_on(async { tunnel.next().await.unwrap().unwrap() });
        assert_eq!(inbound, vec![1]);

        let counter = unsafe { Box::from_raw(counter_ptr) };
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        drop(counter);
    }

    /// Writing one packet whose size alone exceeds the byte limit must return WouldBlock.
    #[test]
    fn write_returns_would_block_when_byte_limit_exceeded() {
        // Use a very small byte limit (4 bytes) but a generous packet-count limit,
        // so only the byte cap is the binding constraint.
        let handle = Arc::new(PacketTunnelHandle::new(
            128, // packet count limit — not the binding constraint
            128, // output (unused)
            4,   // input byte limit: 4 bytes
            DEFAULT_OUTPUT_BYTE_LIMIT,
        ));

        // A 5-byte packet exceeds the 4-byte byte limit.
        let large = vec![0u8; 5];
        let err = handle.push_input_packet(large).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::WouldBlock);
    }

    /// When the queue is byte-limited (but not count-limited), popping a packet
    /// must fire the input-ready notification.
    #[test]
    fn pop_triggers_notification_when_byte_limit_was_binding() {
        let counter = Box::new(AtomicUsize::new(0));
        let counter_ptr = Box::into_raw(counter);

        // byte limit = 3 bytes; count limit = 100 packets (not the binding constraint).
        let handle = Arc::new(PacketTunnelHandle::new(
            100,
            128,
            3, // byte limit
            DEFAULT_OUTPUT_BYTE_LIMIT,
        ));

        // Set the input-ready notifier.
        handle.set_input_ready_notifier(Some(PacketTunnelNotifier {
            rt_id: 0,
            context: counter_ptr.cast(),
            callback: count_notifications,
        }));

        // Push a 3-byte packet — this fills the byte limit exactly.
        handle.push_input_packet(vec![1, 2, 3]).unwrap();

        // A second push (even 1 byte) should be rejected because 3+1 > 3.
        let err = handle.push_input_packet(vec![4]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::WouldBlock);

        // Pop the packet — this should trigger the notification because the queue was
        // byte-full before and is no longer byte-full after.
        let popped = handle.pop_input_packet();
        assert_eq!(popped, Some(vec![1, 2, 3]));

        let counter = unsafe { Box::from_raw(counter_ptr) };
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        drop(counter);
    }

    #[test]
    fn output_capacity_reserves_space_for_one_max_packet() {
        let handle = Arc::new(PacketTunnelHandle::new(
            128,
            128,
            DEFAULT_INPUT_BYTE_LIMIT,
            MAX_IP_PACKET_SIZE + 10,
        ));
        let mut tunnel = PacketTunnelTransport {
            inner: handle.clone(),
        };

        futures::executor::block_on(async { tunnel.send(vec![0u8; 11]).await }).unwrap();

        assert!(!handle.output_has_capacity());

        let mut buffer = vec![0u8; 16];
        match handle.read_output_packet(&mut buffer).unwrap() {
            RuntimePacketRead::Packet(len) => assert_eq!(len, 11),
            _ => panic!("expected packet"),
        }

        assert!(handle.output_has_capacity());
    }
}
