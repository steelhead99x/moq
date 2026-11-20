use std::{
	collections::{HashMap, VecDeque},
	sync::{Arc, Mutex},
	task::Poll,
};

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::{
	Error, PathOwned,
	coding::{Decode, Encode, Reader, Writer},
	ietf::{self, RequestId},
};

use super::{Control, Message, Version};

// === Message Queues ===

struct QueueState<T> {
	messages: VecDeque<T>,
	closed: bool,
}

/// An unbounded FIFO over [`kio::Shared`]: any clone can push or pop.
///
/// Closure is a plain flag rather than handle liveness: [`close`](Self::close)
/// (or dropping a [`QueueWriter`]) stops future pushes, and the reader drains
/// what's buffered before seeing `None`.
struct Queue<T>(kio::Shared<QueueState<T>>);

impl<T> Queue<T> {
	fn new() -> Self {
		Self(kio::Shared::new(QueueState {
			messages: VecDeque::new(),
			closed: false,
		}))
	}

	/// Enqueue a message. Returns false once the queue is closed (the message is
	/// dropped), matching a write to a stream whose reader is gone.
	fn push(&self, item: T) -> bool {
		let mut state = self.0.lock();
		if state.closed {
			return false;
		}
		state.messages.push_back(item);
		true
	}

	/// Dequeue the next message, waiting while the queue is open and empty.
	/// Returns `None` once the queue is closed and drained.
	async fn pop(&self) -> Option<T> {
		kio::wait(|waiter| self.poll_pop(waiter)).await
	}

	/// Poll for the next message; ready with `None` once closed and drained.
	fn poll_pop(&self, waiter: &kio::Waiter) -> Poll<Option<T>> {
		self.0
			.poll(waiter, |state| {
				if state.messages.is_empty() && !state.closed {
					Poll::Pending
				} else {
					Poll::Ready(())
				}
			})
			.map(|mut state| state.messages.pop_front())
	}

	/// Stop future pushes; buffered messages still drain, then `pop` returns `None`.
	fn close(&self) {
		self.0.lock().closed = true;
	}

	/// Mint the single owning push handle whose `Drop` closes the queue.
	fn writer(&self) -> QueueWriter<T> {
		QueueWriter(Self(self.0.clone()))
	}
}

// Manual impls: derives would demand `T: Clone`/`T: Default`, which the queued
// stream pairs don't satisfy.
impl<T> Clone for Queue<T> {
	fn clone(&self) -> Self {
		Self(self.0.clone())
	}
}

impl<T> Default for Queue<T> {
	fn default() -> Self {
		Self::new()
	}
}

/// Single-owner push handle: dropping it closes the queue, which the reading
/// [`VirtualRecvStream`] observes as a FIN after draining.
struct QueueWriter<T>(Queue<T>);

impl<T> QueueWriter<T> {
	/// Enqueue a message, dropped silently if the reader already closed its side.
	fn push(&self, item: T) {
		let _ = self.0.push(item);
	}
}

impl<T> Drop for QueueWriter<T> {
	fn drop(&mut self) {
		self.0.close();
	}
}

// === Virtual Streams ===

/// A virtual receive stream backed by an initial message buffer and a queue of follow-up messages.
pub struct VirtualRecvStream {
	buffer: Bytes,
	rx: Queue<Bytes>,
	closed: bool,
	/// Set once this stream is a request. Dropping the reader releases a namespace
	/// it announced. Empty for streams created before a request id is known, and for
	/// unit tests that never touch the namespace map.
	lease: Arc<Mutex<Option<RequestLease>>>,
}

impl VirtualRecvStream {
	fn new(initial: Bytes, rx: Queue<Bytes>) -> Self {
		Self {
			buffer: initial,
			rx,
			closed: false,
			lease: Arc::new(Mutex::new(None)),
		}
	}

	/// Fill the buffer from the queue if empty. Returns false if the stream is closed.
	async fn fill(&mut self) -> bool {
		if !self.buffer.is_empty() {
			return true;
		}

		if self.closed {
			return false;
		}

		match self.rx.pop().await {
			Some(data) => {
				self.buffer = data;
				true
			}
			None => {
				self.closed = true;
				false
			}
		}
	}
}

impl web_transport_trait::RecvStream for VirtualRecvStream {
	type Error = crate::Error;

	async fn read(&mut self, dst: &mut [u8]) -> Result<Option<usize>, Self::Error> {
		if !self.fill().await {
			return Ok(None);
		}

		let n = dst.len().min(self.buffer.len());
		dst[..n].copy_from_slice(&self.buffer[..n]);
		self.buffer.advance(n);
		Ok(Some(n))
	}

	async fn read_buf<B: BufMut + web_transport_trait::MaybeSend>(
		&mut self,
		buf: &mut B,
	) -> Result<Option<usize>, Self::Error> {
		if !self.fill().await {
			return Ok(None);
		}

		let n = buf.remaining_mut().min(self.buffer.len());
		buf.put(self.buffer.split_to(n));
		Ok(Some(n))
	}

	async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>, Self::Error> {
		if !self.fill().await {
			return Ok(None);
		}

		let n = max.min(self.buffer.len());
		Ok(Some(self.buffer.split_to(n)))
	}

	fn stop(&mut self, _code: u32) {
		self.rx.close();
	}

	async fn closed(&mut self) -> Result<(), Self::Error> {
		// Wait until the queue is closed
		if self.closed {
			return Ok(());
		}
		// Drain remaining messages
		while self.rx.pop().await.is_some() {}
		self.closed = true;
		Ok(())
	}
}

impl Drop for VirtualRecvStream {
	fn drop(&mut self) {
		// The reader is gone: close the queue so routed follow-ups are discarded
		// instead of buffering forever while the map entry lingers.
		self.rx.close();
		// Take the lease so it drops even when the sender still holds the slot.
		// That frees a namespace this request still owns. `Shared::close` releases
		// the same request when the peer ends it first, and the second call is a no-op.
		self.lease.lock().unwrap().take();
	}
}

/// A virtual send stream that forwards writes to the shared control stream writer.
///
/// For streams created by `open_bi` (outgoing requests), this also parses
/// the first outgoing message to extract the request_id and register the
/// stream for response routing.
pub struct VirtualSendStream {
	control_tx: Queue<Bytes>,
	/// Present only for outgoing requests (from open_bi).
	/// Accumulates bytes until the request_id can be parsed,
	/// then registers the stream and flushes.
	pending: Option<OutgoingRegistration>,
}

struct OutgoingRegistration {
	follow_tx: QueueWriter<Bytes>,
	shared: Arc<Shared>,
	version: Version,
	buf: BytesMut,
	/// The matching reader's slot. Filled in once the request id is known, so
	/// dropping that reader releases the namespace.
	lease: Arc<Mutex<Option<RequestLease>>>,
}

impl OutgoingRegistration {
	/// Try to parse the request_id (and optionally namespace) from the accumulated bytes.
	/// Returns Ok(None) if not enough data yet, Err if the message is malformed.
	fn try_parse(&self) -> Result<Option<RequestId>, crate::Error> {
		let mut cursor = std::io::Cursor::new(&self.buf);
		let Ok(type_id) = u64::decode(&mut cursor, self.version) else {
			return Ok(None);
		};
		let Ok(size) = u16::decode(&mut cursor, self.version) else {
			return Ok(None);
		};

		// We know the full message size now: header bytes + body.
		let header_len = cursor.position() as usize;
		let message_len = header_len + size as usize;
		if self.buf.len() < message_len {
			return Ok(None);
		}

		// We have enough bytes for the full message; decoding must succeed.
		let request_id = RequestId::decode(&mut cursor, self.version)?;

		// For PublishNamespace, also extract the namespace for reverse lookup.
		if type_id == ietf::PublishNamespace::ID {
			if self.version == Version::Draft17 {
				// v17 has required_request_id_delta after request_id
				let _ = u64::decode(&mut cursor, self.version);
			}
			if let Ok(ns) = crate::ietf::namespace::decode_namespace(&mut cursor, self.version) {
				self.shared.namespaces.insert(Direction::Outgoing, ns, request_id);
			}
		}

		Ok(Some(request_id))
	}

	fn register(self, request_id: RequestId) {
		self.shared.streams.lock().unwrap().insert(request_id, self.follow_tx);
		// Every outgoing request gets a lease. Only a namespace announcement has
		// a map entry for `release` to find; the rest are a no-op.
		*self.lease.lock().unwrap() = Some(RequestLease {
			namespaces: Arc::clone(&self.shared.namespaces),
			request_id,
		});
	}
}

impl VirtualSendStream {
	fn new(control_tx: Queue<Bytes>) -> Self {
		Self {
			control_tx,
			pending: None,
		}
	}

	fn with_registration(control_tx: Queue<Bytes>, pending: OutgoingRegistration) -> Self {
		Self {
			control_tx,
			pending: Some(pending),
		}
	}
}

impl web_transport_trait::SendStream for VirtualSendStream {
	type Error = crate::Error;

	async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
		let len = buf.len();

		if let Some(pending) = &mut self.pending {
			pending.buf.extend_from_slice(buf);

			if let Some(request_id) = pending.try_parse()? {
				let mut pending = self.pending.take().unwrap();
				let buf = std::mem::take(&mut pending.buf).freeze();
				pending.register(request_id);
				if !self.control_tx.push(buf) {
					return Err(crate::Error::Closed);
				}
			}
		} else if !self.control_tx.push(Bytes::copy_from_slice(buf)) {
			return Err(crate::Error::Closed);
		}

		Ok(len)
	}

	async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), Self::Error> {
		if let Some(pending) = &mut self.pending {
			pending.buf.extend_from_slice(&chunk);

			if let Some(request_id) = pending.try_parse()? {
				let mut pending = self.pending.take().unwrap();
				let buf = std::mem::take(&mut pending.buf).freeze();
				pending.register(request_id);
				if !self.control_tx.push(buf) {
					return Err(crate::Error::Closed);
				}
			}
		} else if !self.control_tx.push(chunk) {
			return Err(crate::Error::Closed);
		}

		Ok(())
	}

	fn set_priority(&mut self, _order: u8) {}

	fn finish(&mut self) -> Result<(), Self::Error> {
		// Flush any remaining buffered data (e.g. if registration never completed).
		if let Some(pending) = self.pending.take()
			&& !pending.buf.is_empty()
		{
			let _ = self.control_tx.push(pending.buf.freeze());
		}
		Ok(())
	}

	fn reset(&mut self, _code: u32) {}

	async fn closed(&mut self) -> Result<(), Self::Error> {
		Ok(())
	}
}

// === Adapter Send/Recv Enums ===

pub enum AdapterSend<S: web_transport_trait::Session> {
	Real(S::SendStream),
	Virtual(VirtualSendStream),
}

impl<S: web_transport_trait::Session> web_transport_trait::SendStream for AdapterSend<S> {
	type Error = crate::Error;

	async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
		match self {
			Self::Real(s) => s.write(buf).await.map_err(|_| crate::Error::Closed),
			Self::Virtual(s) => s.write(buf).await,
		}
	}

	async fn write_buf<B: Buf + web_transport_trait::MaybeSend>(&mut self, buf: &mut B) -> Result<usize, Self::Error> {
		match self {
			Self::Real(s) => s.write_buf(buf).await.map_err(|_| crate::Error::Closed),
			Self::Virtual(s) => s.write_buf(buf).await,
		}
	}

	async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), Self::Error> {
		match self {
			Self::Real(s) => s.write_chunk(chunk).await.map_err(|_| crate::Error::Closed),
			Self::Virtual(s) => s.write_chunk(chunk).await,
		}
	}

	fn set_priority(&mut self, order: u8) {
		match self {
			Self::Real(s) => s.set_priority(order),
			Self::Virtual(s) => s.set_priority(order),
		}
	}

	fn finish(&mut self) -> Result<(), Self::Error> {
		match self {
			Self::Real(s) => s.finish().map_err(|_| crate::Error::Closed),
			Self::Virtual(s) => s.finish(),
		}
	}

	fn reset(&mut self, code: u32) {
		match self {
			Self::Real(s) => s.reset(code),
			Self::Virtual(s) => s.reset(code),
		}
	}

	async fn closed(&mut self) -> Result<(), Self::Error> {
		match self {
			Self::Real(s) => s.closed().await.map_err(|_| crate::Error::Closed),
			Self::Virtual(s) => s.closed().await,
		}
	}
}

pub enum AdapterRecv<S: web_transport_trait::Session> {
	Real(S::RecvStream),
	Virtual(VirtualRecvStream),
}

impl<S: web_transport_trait::Session> web_transport_trait::RecvStream for AdapterRecv<S> {
	type Error = crate::Error;

	async fn read(&mut self, dst: &mut [u8]) -> Result<Option<usize>, Self::Error> {
		match self {
			Self::Real(s) => s.read(dst).await.map_err(|_| crate::Error::Closed),
			Self::Virtual(s) => s.read(dst).await,
		}
	}

	async fn read_buf<B: BufMut + web_transport_trait::MaybeSend>(
		&mut self,
		buf: &mut B,
	) -> Result<Option<usize>, Self::Error> {
		match self {
			Self::Real(s) => s.read_buf(buf).await.map_err(|_| crate::Error::Closed),
			Self::Virtual(s) => s.read_buf(buf).await,
		}
	}

	async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>, Self::Error> {
		match self {
			Self::Real(s) => s.read_chunk(max).await.map_err(|_| crate::Error::Closed),
			Self::Virtual(s) => s.read_chunk(max).await,
		}
	}

	fn stop(&mut self, code: u32) {
		match self {
			Self::Real(s) => s.stop(code),
			Self::Virtual(s) => s.stop(code),
		}
	}

	async fn closed(&mut self) -> Result<(), Self::Error> {
		match self {
			Self::Real(s) => s.closed().await.map_err(|_| crate::Error::Closed),
			Self::Virtual(s) => s.closed().await,
		}
	}
}

// === Control Stream Adapter ===

/// Which side of the session advertised a namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
	/// We advertised it to the peer, on a stream from `open_bi`.
	Outgoing,
	/// The peer advertised it to us, on a stream handed to `accept_bi`.
	Incoming,
}

/// One request's registration, including a duplicate the session will refuse.
///
/// The forward maps keep only the live announcement. This record is what lets
/// [`Namespaces::release`] tell a refused duplicate from the request that still
/// owns the name.
struct Registration {
	direction: Direction,
	namespace: PathOwned,
}

/// Namespace to request_id lookup for the v14/v15 messages that name a
/// namespace instead of a request id.
///
/// Direction is part of the key because a cluster mesh has both endpoints
/// advertising the same namespace to each other. One map would let the second
/// advertisement overwrite the first, and a terminal message would then tear
/// down the stream travelling the other way while its real target stayed open.
///
/// Within one direction the first announcement wins. A later `PUBLISH_NAMESPACE`
/// for a live name is still delivered (the session may refuse it), but it must
/// not replace the request a withdrawal will close.
#[derive(Default)]
struct Namespaces {
	outgoing: Mutex<HashMap<PathOwned, RequestId>>,
	incoming: Mutex<HashMap<PathOwned, RequestId>>,
	/// Every request that announced a namespace, owner and refused duplicate alike.
	by_request: Mutex<HashMap<RequestId, Registration>>,
}

/// Drops one request's namespace registration when its reader goes away.
///
/// [`Shared::close`] releases the same request when the peer ends it, so the
/// name is free while the local reader is still holding the FIN'd stream. A
/// second release is a no-op.
struct RequestLease {
	namespaces: Arc<Namespaces>,
	request_id: RequestId,
}

impl Drop for RequestLease {
	fn drop(&mut self) {
		self.namespaces.release(self.request_id);
	}
}

impl Namespaces {
	fn map(&self, direction: Direction) -> &Mutex<HashMap<PathOwned, RequestId>> {
		match direction {
			Direction::Outgoing => &self.outgoing,
			Direction::Incoming => &self.incoming,
		}
	}

	/// Record `request_id` as an announcement of `namespace` in `direction`.
	///
	/// The request is always remembered, so closing a refused duplicate cannot
	/// retract the live name. The forward map keeps the first request that
	/// claimed the name.
	fn insert(&self, direction: Direction, namespace: PathOwned, request_id: RequestId) {
		self.by_request.lock().unwrap().insert(
			request_id,
			Registration {
				direction,
				namespace: namespace.clone(),
			},
		);
		self.map(direction)
			.lock()
			.unwrap()
			.entry(namespace)
			.or_insert(request_id);
	}

	fn get(&self, direction: Direction, namespace: &PathOwned) -> Option<RequestId> {
		self.map(direction).lock().unwrap().get(namespace).copied()
	}

	/// Drop `request_id`'s registration. The live name is removed only when this
	/// request still owns it.
	fn release(&self, request_id: RequestId) {
		let Some(registered) = self.by_request.lock().unwrap().remove(&request_id) else {
			return;
		};
		let mut map = self.map(registered.direction).lock().unwrap();
		if map.get(&registered.namespace).copied() == Some(request_id) {
			map.remove(&registered.namespace);
		}
	}
}

#[derive(Default)]
struct Shared {
	/// New virtual streams for accept_bi; any caller can pop directly.
	incoming: Queue<(VirtualSendStream, VirtualRecvStream)>,

	/// Messages every VirtualSendStream writes; the run_write task drains this
	/// onto the real control stream.
	control: Queue<Bytes>,

	/// Active virtual streams keyed by request_id. Removing an entry drops its
	/// writer, which FINs the matching VirtualRecvStream.
	streams: Mutex<HashMap<RequestId, QueueWriter<Bytes>>>,

	/// Namespace → request_id reverse lookup (for v14/v15 namespace-keyed messages).
	///
	/// Shared with the per-request leases so a dropped reader can release its name
	/// without holding the whole session.
	namespaces: Arc<Namespaces>,
}

impl Shared {
	/// Mint the local half of a request we're about to send. Registration waits
	/// until the first write reveals the request_id.
	fn open_outgoing(self: &Arc<Self>, version: Version) -> (VirtualSendStream, VirtualRecvStream) {
		let follow = Queue::new();
		let recv = VirtualRecvStream::new(Bytes::new(), follow.clone());
		let send = VirtualSendStream::with_registration(
			self.control.clone(),
			OutgoingRegistration {
				follow_tx: follow.writer(),
				shared: Arc::clone(self),
				version,
				buf: BytesMut::new(),
				lease: Arc::clone(&recv.lease),
			},
		);
		(send, recv)
	}

	/// Register the peer's new request and queue its stream for accept_bi.
	fn open_incoming(&self, request_id: RequestId, raw: Bytes) -> Result<(), Error> {
		let follow = Queue::new();
		let recv = VirtualRecvStream::new(raw, follow.clone());
		// The reader dropping is one way a request ends. `close` is the other.
		// A request that never announced a namespace releases nothing.
		*recv.lease.lock().unwrap() = Some(RequestLease {
			namespaces: Arc::clone(&self.namespaces),
			request_id,
		});
		let send = VirtualSendStream::new(self.control.clone());
		self.streams.lock().unwrap().insert(request_id, follow.writer());
		if !self.incoming.push((send, recv)) {
			return Err(Error::Closed);
		}
		Ok(())
	}

	/// Deliver a message to an open virtual stream, dropping it if the stream is gone.
	fn push(&self, request_id: RequestId, raw: Bytes) {
		if let Some(tx) = self.streams.lock().unwrap().get(&request_id) {
			tx.push(raw);
		}
	}

	/// Deliver a final message and FIN the stream: the removed writer drops after it.
	///
	/// The namespace is released even when the stream is already gone, and even
	/// while the reader is still holding the FIN'd stream. A refused duplicate
	/// releases only itself, so the request that still owns the name stays up.
	fn close(&self, request_id: RequestId, raw: Bytes) {
		self.namespaces.release(request_id);
		if let Some(tx) = self.streams.lock().unwrap().remove(&request_id) {
			tx.push(raw);
		}
	}
}

#[derive(Clone)]
pub struct ControlStreamAdapter<S: web_transport_trait::Session> {
	inner: S,
	shared: Arc<Shared>,
	control: Control,
	version: Version,
}

impl<S: web_transport_trait::Session> ControlStreamAdapter<S> {
	pub fn new(inner: S, control: Control, version: Version) -> Self {
		Self {
			inner,
			shared: Arc::new(Shared::default()),
			control,
			version,
		}
	}

	/// Open a real (non-virtual) bidi stream, bypassing control stream multiplexing.
	/// Used for v16 SubscribeNamespace which moved to its own bidi stream.
	pub async fn open_native_bi(&self) -> Result<(AdapterSend<S>, AdapterRecv<S>), crate::Error> {
		let (send, recv) = self.inner.open_bi().await.map_err(|_| crate::Error::Closed)?;
		Ok((AdapterSend::Real(send), AdapterRecv::Real(recv)))
	}

	/// Run the control stream read + write tasks.
	/// This reads from the control stream and routes messages to virtual streams,
	/// and also drains the write queue to the control stream writer.
	pub async fn run(
		&self,
		reader: Reader<S::RecvStream, Version>,
		writer: Writer<S::SendStream, Version>,
	) -> Result<(), Error> {
		let res = {
			let mut read = std::pin::pin!(self.run_read(reader));
			let mut write = std::pin::pin!(self.run_write(writer));
			kio::wait(|waiter| {
				if let Poll::Ready(res) = waiter.poll_future(read.as_mut()) {
					return Poll::Ready(res);
				}
				if let Poll::Ready(res) = waiter.poll_future(write.as_mut()) {
					return Poll::Ready(res);
				}
				Poll::Pending
			})
			.await
		};

		// The control stream is dead, so the mux is too: fail future virtual
		// writes fast and end accept_bi instead of buffering into a queue nobody
		// will ever drain.
		self.shared.control.close();
		self.shared.incoming.close();

		res
	}

	/// Writer task: drains the queue and writes to the control stream.
	async fn run_write(&self, mut writer: Writer<S::SendStream, Version>) -> Result<(), Error> {
		while let Some(msg) = self.shared.control.pop().await {
			let mut buf = std::io::Cursor::new(msg);
			writer.write_all(&mut buf).await?;
		}
		Ok(())
	}

	/// Dispatcher loop that reads control stream messages and routes them.
	async fn run_read(&self, mut reader: Reader<S::RecvStream, Version>) -> Result<(), Error> {
		loop {
			let type_id: u64 = match reader.decode_maybe().await? {
				Some(id) => id,
				None => return Ok(()),
			};

			let size: u16 = reader.decode::<u16>().await?;

			let body = reader.read_exact(size as usize).await?;

			// Reconstruct raw message bytes: [type_id][size][body]
			let raw = encode_raw(type_id, size, &body, self.version);

			// Classify and route
			match classify(type_id, &body, self.version, &self.shared.namespaces)? {
				Route::NewRequest(request_id) => self.shared.open_incoming(request_id, raw)?,
				Route::Response(request_id) | Route::FollowUp(request_id) => self.shared.push(request_id, raw),
				Route::CloseStream(request_id) => self.shared.close(request_id, raw),
				// A v14/v15 withdrawal for a name we are not announcing. The message
				// is consumed and the control stream stays up.
				Route::Ignore => {}
				Route::MaxRequestId(max) => self.control.max_request_id(max),
				Route::GoAway => return Err(Error::Unsupported),
			}
		}
	}
}

/// Classify a control message and extract its request_id for routing.
///
/// Takes the namespace map rather than reading it off the adapter so the routing
/// rules can be tested without a session behind them.
fn classify(type_id: u64, body: &Bytes, version: Version, namespaces: &Namespaces) -> Result<Route, Error> {
	match type_id {
		// New requests: these create new virtual streams
		ietf::Subscribe::ID => {
			let id = decode_request_id(body, version)?;
			Ok(Route::NewRequest(id))
		}
		ietf::Fetch::ID => {
			let id = decode_request_id(body, version)?;
			Ok(Route::NewRequest(id))
		}
		ietf::Publish::ID => {
			let id = decode_request_id(body, version)?;
			Ok(Route::NewRequest(id))
		}
		ietf::PublishNamespace::ID => {
			let id = decode_request_id(body, version)?;
			// Decode the namespace and store the mapping for v14/v15 reverse lookup
			if let Ok(ns) = decode_publish_namespace_body(body, version) {
				namespaces.insert(Direction::Incoming, ns, id);
			}
			Ok(Route::NewRequest(id))
		}
		ietf::TrackStatus::ID => {
			let id = decode_request_id(body, version)?;
			Ok(Route::NewRequest(id))
		}
		// SubscribeNamespace on control stream (v14/v15 only)
		ietf::SubscribeNamespaceLegacy::ID => match version {
			Version::Draft14 | Version::Draft15 => {
				let id = decode_request_id(body, version)?;
				Ok(Route::NewRequest(id))
			}
			_ => Err(Error::UnexpectedMessage),
		},

		// SUBSCRIBE_TRACKS (draft-18+, #1542): the half of the SUBSCRIBE_NAMESPACE split
		// that subscribes to all tracks under a prefix. We don't implement PUBLISH
		// replication, so this is impossible to honor. Reject loudly.
		ietf::SUBSCRIBE_TRACKS_ID => {
			tracing::error!(
				version = ?version,
				"received SUBSCRIBE_TRACKS (0x51); not supported in moq-lite (no PUBLISH replication)"
			);
			Err(Error::Unsupported)
		}

		// Responses: route to the virtual stream waiting for a reply
		ietf::SubscribeOk::ID => {
			let id = decode_response_request_id(body, version)?;
			Ok(Route::Response(id))
		}
		// 0x05: SubscribeError in v14, RequestError in v15+
		ietf::SubscribeError::ID => {
			let id = decode_response_request_id(body, version)?;
			Ok(Route::CloseStream(id))
		}
		ietf::FetchOk::ID => {
			let id = decode_response_request_id(body, version)?;
			Ok(Route::Response(id))
		}
		// 0x19: FetchError in v14 only
		ietf::FetchError::ID => match version {
			Version::Draft14 => {
				let id = decode_request_id(body, version)?;
				Ok(Route::CloseStream(id))
			}
			_ => Err(Error::UnexpectedMessage),
		},
		// PublishOk (0x1E)
		ietf::PublishOk::ID => {
			let id = decode_response_request_id(body, version)?;
			Ok(Route::Response(id))
		}
		// PublishError (0x1F) - v14 only
		ietf::PublishError::ID => {
			let id = decode_request_id(body, version)?;
			Ok(Route::CloseStream(id))
		}
		// 0x07: PublishNamespaceOk in v14, RequestOk in v15+
		ietf::PublishNamespaceOk::ID => match version {
			Version::Draft14 => {
				let id = decode_request_id(body, version)?;
				Ok(Route::Response(id))
			}
			Version::Draft15 | Version::Draft16 => {
				// RequestOk - route to stream
				let id = decode_response_request_id(body, version)?;
				Ok(Route::Response(id))
			}
			_ => Err(Error::UnexpectedMessage),
		},
		// 0x08: PublishNamespaceError in v14 only
		ietf::PublishNamespaceError::ID => match version {
			Version::Draft14 => {
				let id = decode_request_id(body, version)?;
				Ok(Route::CloseStream(id))
			}
			_ => Err(Error::UnexpectedMessage),
		},
		// SubscribeNamespaceOk (v14 only)
		ietf::SubscribeNamespaceOk::ID => match version {
			Version::Draft14 => {
				let id = decode_request_id(body, version)?;
				Ok(Route::Response(id))
			}
			_ => Err(Error::UnexpectedMessage),
		},
		// SubscribeNamespaceError (v14 only)
		ietf::SubscribeNamespaceError::ID => match version {
			Version::Draft14 => {
				let id = decode_request_id(body, version)?;
				Ok(Route::CloseStream(id))
			}
			_ => Err(Error::UnexpectedMessage),
		},

		// Follow-up messages: route to existing stream
		ietf::SubscribeUpdate::ID => {
			let id = decode_request_id(body, version)?;
			Ok(Route::FollowUp(id))
		}

		// Close stream messages
		ietf::Unsubscribe::ID => {
			let id = decode_request_id(body, version)?;
			Ok(Route::CloseStream(id))
		}
		ietf::PublishDone::ID => {
			let id = decode_response_request_id(body, version)?;
			Ok(Route::CloseStream(id))
		}
		ietf::FetchCancel::ID => {
			let id = decode_request_id(body, version)?;
			Ok(Route::CloseStream(id))
		}
		ietf::PublishNamespaceDone::ID => match version {
			Version::Draft16 => {
				let id = decode_request_id(body, version)?;
				Ok(Route::CloseStream(id))
			}
			// v14/v15: namespace-keyed, so decode namespace and look up request_id.
			// DONE withdraws the advertisement its sender made, so it names an
			// inbound one. A name we have no announcement for is dropped: it
			// belongs to a duplicate already refused, or to a request already gone.
			Version::Draft14 | Version::Draft15 => {
				match lookup_namespace_request_id(body, version, namespaces, Direction::Incoming)? {
					Some(id) => Ok(Route::CloseStream(id)),
					None => Ok(Route::Ignore),
				}
			}
			_ => Err(Error::UnexpectedMessage),
		},
		ietf::PublishNamespaceCancel::ID => match version {
			Version::Draft16 => {
				let id = decode_request_id(body, version)?;
				Ok(Route::CloseStream(id))
			}
			// v14/v15: namespace-keyed. CANCEL rejects an advertisement its sender
			// received, so it names an outbound one. An unknown name is dropped
			// rather than failing the session.
			Version::Draft14 | Version::Draft15 => {
				match lookup_namespace_request_id(body, version, namespaces, Direction::Outgoing)? {
					Some(id) => Ok(Route::CloseStream(id)),
					None => Ok(Route::Ignore),
				}
			}
			_ => Err(Error::UnexpectedMessage),
		},
		ietf::UnsubscribeNamespace::ID => match version {
			Version::Draft14 | Version::Draft15 => {
				let id = decode_request_id(body, version)?;
				Ok(Route::CloseStream(id))
			}
			_ => Err(Error::UnexpectedMessage),
		},

		// Utility
		ietf::MaxRequestId::ID => {
			let id = decode_request_id(body, version)?;
			Ok(Route::MaxRequestId(id))
		}
		ietf::RequestsBlocked::ID => Err(Error::UnexpectedMessage),

		// Terminal
		ietf::GoAway::ID => Ok(Route::GoAway),

		_ => Err(Error::UnexpectedMessage),
	}
}

/// Decode the namespace from a v14/v15 namespace-keyed message body and look up the
/// request_id of the advertisement it terminates, travelling in the given direction.
/// Decode the namespace from a v14/v15 namespace-keyed message body and look up
/// the request_id of the advertisement it terminates.
///
/// `Ok(None)` is a name with no live announcement. That is not a session error:
/// failing the control stream here would take every healthy request with it.
fn lookup_namespace_request_id(
	body: &Bytes,
	version: Version,
	namespaces: &Namespaces,
	direction: Direction,
) -> Result<Option<RequestId>, Error> {
	let mut cursor = std::io::Cursor::new(body);
	let ns = crate::ietf::namespace::decode_namespace(&mut cursor, version)?;
	if let Some(id) = namespaces.get(direction, &ns) {
		return Ok(Some(id));
	}
	tracing::warn!(
		namespace = %ns,
		?direction,
		"dropping namespace withdrawal with no announcement"
	);
	Ok(None)
}

impl<S: web_transport_trait::Session> web_transport_trait::Session for ControlStreamAdapter<S> {
	type SendStream = AdapterSend<S>;
	type RecvStream = AdapterRecv<S>;
	type Error = crate::Error;

	async fn accept_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), Self::Error> {
		match self.version {
			// v16: SubscribeNamespace uses real bidi streams, so race both sources.
			Version::Draft16 => {
				// Native first: real bidi streams are rare and transport-paced, so
				// they can't starve the queue, while a control-message flood could
				// starve a native SubscribeNamespace under queue-first order.
				let mut accept = std::pin::pin!(self.inner.accept_bi());
				kio::wait(|waiter| {
					if let Poll::Ready(result) = waiter.poll_future(accept.as_mut()) {
						return Poll::Ready(match result {
							Ok((send, recv)) => Ok((AdapterSend::Real(send), AdapterRecv::Real(recv))),
							Err(_) => Err(crate::Error::Closed),
						});
					}
					if let Poll::Ready(result) = self.shared.incoming.poll_pop(waiter) {
						return Poll::Ready(match result {
							Some((send, recv)) => Ok((AdapterSend::Virtual(send), AdapterRecv::Virtual(recv))),
							None => Err(crate::Error::Closed),
						});
					}
					Poll::Pending
				})
				.await
			}
			// v14/v15: Only virtual streams from control stream.
			_ => match self.shared.incoming.pop().await {
				Some((send, recv)) => Ok((AdapterSend::Virtual(send), AdapterRecv::Virtual(recv))),
				None => Err(crate::Error::Closed),
			},
		}
	}

	async fn open_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), Self::Error> {
		let (send, recv) = self.shared.open_outgoing(self.version);
		Ok((AdapterSend::Virtual(send), AdapterRecv::Virtual(recv)))
	}

	async fn open_uni(&self) -> Result<Self::SendStream, Self::Error> {
		let s = self.inner.open_uni().await.map_err(|_| crate::Error::Closed)?;
		Ok(AdapterSend::Real(s))
	}

	async fn accept_uni(&self) -> Result<Self::RecvStream, Self::Error> {
		let s = self.inner.accept_uni().await.map_err(|_| crate::Error::Closed)?;
		Ok(AdapterRecv::Real(s))
	}

	fn send_datagram(&self, payload: Bytes) -> Result<(), Self::Error> {
		self.inner.send_datagram(payload).map_err(|_| crate::Error::Closed)
	}

	async fn recv_datagram(&self) -> Result<Bytes, Self::Error> {
		self.inner.recv_datagram().await.map_err(|_| crate::Error::Closed)
	}

	fn max_datagram_size(&self) -> usize {
		self.inner.max_datagram_size()
	}

	fn protocol(&self) -> Option<&str> {
		self.inner.protocol()
	}

	fn close(&self, code: u32, reason: &str) {
		self.inner.close(code, reason)
	}

	async fn closed(&self) -> Self::Error {
		let _ = self.inner.closed().await;
		crate::Error::Closed
	}
}

// === Message Classification ===

#[derive(Debug)]
enum Route {
	NewRequest(RequestId),
	Response(RequestId),
	FollowUp(RequestId),
	CloseStream(RequestId),
	/// A v14/v15 withdrawal that names a namespace we are not announcing.
	/// The control stream stays up.
	Ignore,
	MaxRequestId(RequestId),
	GoAway,
}

/// Encode raw message bytes as [type_id varint][size u16][body].
fn encode_raw(type_id: u64, size: u16, body: &Bytes, version: Version) -> Bytes {
	let mut buf = BytesMut::new();
	type_id.encode(&mut buf, version).expect("encode type_id");
	size.encode(&mut buf, version).expect("encode size");
	buf.extend_from_slice(body);
	buf.freeze()
}

/// Decode just the request_id from the beginning of a message body.
fn decode_request_id(body: &Bytes, version: Version) -> Result<RequestId, Error> {
	let mut cursor = std::io::Cursor::new(body);
	let request_id = RequestId::decode(&mut cursor, version)?;
	Ok(request_id)
}

/// Decode request_id for response messages that have Option<RequestId> in v14-16.
fn decode_response_request_id(body: &Bytes, version: Version) -> Result<RequestId, Error> {
	// In v14-16, response messages always have request_id present
	decode_request_id(body, version)
}

/// Decode the namespace from a PublishNamespace message body (after the request_id).
fn decode_publish_namespace_body(body: &Bytes, version: Version) -> Result<PathOwned, Error> {
	let mut cursor = std::io::Cursor::new(body);
	// Skip request_id
	let _request_id = RequestId::decode(&mut cursor, version)?;
	// v17 has required_request_id_delta
	if version == Version::Draft17 {
		let _ = u64::decode(&mut cursor, version)?;
	}
	let ns = crate::ietf::namespace::decode_namespace(&mut cursor, version)?;
	Ok(ns.into_owned())
}

#[cfg(test)]
mod tests {
	use super::*;
	use bytes::BytesMut;
	use futures::FutureExt as _;
	use web_transport_trait::{RecvStream as _, SendStream as _};

	fn make_body_with_request_id(id: u64, version: Version) -> Bytes {
		let mut buf = BytesMut::new();
		RequestId(id).encode(&mut buf, version).unwrap();
		buf.freeze()
	}

	/// Classify against an empty namespace map, for the messages that don't use it.
	fn classify_msg(version: Version, type_id: u64, body: &Bytes) -> Result<Route, Error> {
		classify(type_id, body, version, &Namespaces::default())
	}

	#[test]
	fn test_classify_subscribe_new_request() {
		let body = make_body_with_request_id(42, Version::Draft15);
		let route = classify_msg(Version::Draft15, ietf::Subscribe::ID, &body).unwrap();
		assert!(matches!(route, Route::NewRequest(RequestId(42))));
	}

	#[test]
	fn test_classify_fetch_new_request() {
		let body = make_body_with_request_id(10, Version::Draft14);
		let route = classify_msg(Version::Draft14, ietf::Fetch::ID, &body).unwrap();
		assert!(matches!(route, Route::NewRequest(RequestId(10))));
	}

	#[test]
	fn test_classify_publish_new_request() {
		let body = make_body_with_request_id(5, Version::Draft16);
		let route = classify_msg(Version::Draft16, ietf::Publish::ID, &body).unwrap();
		assert!(matches!(route, Route::NewRequest(RequestId(5))));
	}

	#[test]
	fn test_classify_subscribe_ok_response() {
		let body = make_body_with_request_id(42, Version::Draft15);
		let route = classify_msg(Version::Draft15, ietf::SubscribeOk::ID, &body).unwrap();
		assert!(matches!(route, Route::Response(RequestId(42))));
	}

	#[test]
	fn test_classify_request_error_v15_closes_stream() {
		let body = make_body_with_request_id(7, Version::Draft15);
		let route = classify_msg(Version::Draft15, ietf::SubscribeError::ID, &body).unwrap();
		assert!(matches!(route, Route::CloseStream(RequestId(7))));
	}

	#[test]
	fn test_classify_request_ok_v15_response() {
		let body = make_body_with_request_id(3, Version::Draft15);
		let route = classify_msg(Version::Draft15, ietf::PublishNamespaceOk::ID, &body).unwrap();
		assert!(matches!(route, Route::Response(RequestId(3))));
	}

	#[test]
	fn test_classify_unsubscribe_closes_stream() {
		let body = make_body_with_request_id(99, Version::Draft14);
		let route = classify_msg(Version::Draft14, ietf::Unsubscribe::ID, &body).unwrap();
		assert!(matches!(route, Route::CloseStream(RequestId(99))));
	}

	#[test]
	fn test_classify_subscribe_update_followup() {
		let body = make_body_with_request_id(10, Version::Draft15);
		let route = classify_msg(Version::Draft15, ietf::SubscribeUpdate::ID, &body).unwrap();
		assert!(matches!(route, Route::FollowUp(RequestId(10))));
	}

	#[test]
	fn test_classify_goaway() {
		let body = Bytes::new();
		let route = classify_msg(Version::Draft14, ietf::GoAway::ID, &body).unwrap();
		assert!(matches!(route, Route::GoAway));
	}

	#[test]
	fn test_classify_max_request_id() {
		let body = make_body_with_request_id(100, Version::Draft14);
		let route = classify_msg(Version::Draft14, ietf::MaxRequestId::ID, &body).unwrap();
		assert!(matches!(route, Route::MaxRequestId(RequestId(100))));
	}

	#[test]
	fn test_classify_subscribe_namespace_v14_new_request() {
		let body = make_body_with_request_id(20, Version::Draft14);
		let route = classify_msg(Version::Draft14, ietf::SubscribeNamespaceLegacy::ID, &body).unwrap();
		assert!(matches!(route, Route::NewRequest(RequestId(20))));
	}

	#[test]
	fn test_classify_subscribe_namespace_v16_errors() {
		let body = make_body_with_request_id(20, Version::Draft16);
		let result = classify_msg(Version::Draft16, ietf::SubscribeNamespaceLegacy::ID, &body);
		assert!(result.is_err());
	}

	#[test]
	fn test_classify_unknown_message() {
		let body = Bytes::new();
		let result = classify_msg(Version::Draft14, 0xFF, &body);
		assert!(result.is_err());
	}

	#[test]
	fn test_encode_raw_roundtrip() {
		let version = Version::Draft15;
		let body = Bytes::from_static(b"hello");
		let raw = encode_raw(0x03, 5, &body, version);

		// Decode the raw bytes
		let mut cursor = std::io::Cursor::new(&raw[..]);
		let type_id = u64::decode(&mut cursor, version).unwrap();
		let size = u16::decode(&mut cursor, version).unwrap();
		assert_eq!(type_id, 0x03);
		assert_eq!(size, 5);
	}

	#[tokio::test]
	async fn test_virtual_recv_stream_reads_initial_then_followup() {
		let initial = Bytes::from_static(b"initial");
		let follow = Queue::new();
		let tx = follow.writer();
		let mut stream = VirtualRecvStream::new(initial, follow);

		// Read initial data
		let mut buf = [0u8; 32];
		let n = stream.read(&mut buf).await.unwrap().unwrap();
		assert_eq!(&buf[..n], b"initial");

		// Send follow-up
		tx.push(Bytes::from_static(b"followup"));
		let n = stream.read(&mut buf).await.unwrap().unwrap();
		assert_eq!(&buf[..n], b"followup");

		// Drop the writer → FIN
		drop(tx);
		let result = stream.read(&mut buf).await.unwrap();
		assert_eq!(result, None);
	}

	#[tokio::test]
	async fn test_virtual_recv_stream_partial_reads() {
		let initial = Bytes::from_static(b"hello world");
		let mut stream = VirtualRecvStream::new(initial, Queue::new());

		// Read small chunks
		let mut buf = [0u8; 5];
		let n = stream.read(&mut buf).await.unwrap().unwrap();
		assert_eq!(&buf[..n], b"hello");

		let n = stream.read(&mut buf).await.unwrap().unwrap();
		assert_eq!(&buf[..n], b" worl");

		let mut buf = [0u8; 1];
		let n = stream.read(&mut buf).await.unwrap().unwrap();
		assert_eq!(&buf[..n], b"d");
	}

	#[tokio::test]
	async fn test_virtual_send_stream_errors_once_control_closes() {
		// The adapter closes the control queue when its run() exits; writes must
		// fail fast instead of buffering into a queue nobody drains.
		let control = Queue::new();
		let mut stream = VirtualSendStream::new(control.clone());
		control.close();
		assert!(matches!(stream.write(b"hello").await, Err(Error::Closed)));
	}

	#[test]
	fn test_recv_stream_drop_discards_followups() {
		// Dropping the reader closes the queue, so late routed messages are
		// discarded instead of accumulating while the map entry lingers.
		let follow = Queue::new();
		let stream = VirtualRecvStream::new(Bytes::new(), follow.clone());
		drop(stream);
		assert!(!follow.push(Bytes::from_static(b"late")));
	}

	#[tokio::test]
	async fn test_virtual_send_stream_writes_to_channel() {
		let control = Queue::new();
		let mut stream = VirtualSendStream::new(control.clone());

		let n = stream.write(b"hello").await.unwrap();
		assert_eq!(n, 5);

		let data = control.pop().await.unwrap();
		assert_eq!(data, &b"hello"[..]);
	}

	/// Encode a message body (no type_id/size header).
	fn encode_body<M: Message>(msg: &M, version: Version) -> Bytes {
		let mut buf = BytesMut::new();
		msg.encode_msg(&mut buf, version).unwrap();
		buf.freeze()
	}

	/// Encode a full control message: [type_id][size][body].
	fn encode_msg<M: Message>(msg: &M, version: Version) -> Bytes {
		let body = encode_body(msg, version);
		encode_raw(M::ID, body.len() as u16, &body, version)
	}

	fn publish_namespace(request_id: RequestId, namespace: &str) -> ietf::PublishNamespace<'_> {
		ietf::PublishNamespace {
			request_id,
			track_namespace: crate::Path::new(namespace),
			cluster: None,
		}
	}

	/// Advertise `namespace` to the peer on request 4, as `open_bi` + a write would.
	async fn advertise(shared: &Arc<Shared>, namespace: &str, version: Version) -> VirtualRecvStream {
		advertise_as(shared, namespace, version, RequestId(4)).await
	}

	/// Advertise `namespace` on `request_id`, as `open_bi` + a write would.
	async fn advertise_as(
		shared: &Arc<Shared>,
		namespace: &str,
		version: Version,
		request_id: RequestId,
	) -> VirtualRecvStream {
		let (mut send, recv) = shared.open_outgoing(version);
		send.write_chunk(encode_msg(&publish_namespace(request_id, namespace), version))
			.await
			.unwrap();
		recv
	}

	/// Receive the peer's advertisement of `namespace` on request 7, as the read loop would.
	async fn receive(shared: &Arc<Shared>, namespace: &str, version: Version) -> VirtualRecvStream {
		receive_as(shared, namespace, version, RequestId(7)).await
	}

	/// Receive the peer's advertisement of `namespace` on `request_id`, as the read loop would.
	async fn receive_as(
		shared: &Arc<Shared>,
		namespace: &str,
		version: Version,
		request_id: RequestId,
	) -> VirtualRecvStream {
		let msg = publish_namespace(request_id, namespace);
		let route = classify(
			ietf::PublishNamespace::ID,
			&encode_body(&msg, version),
			version,
			&shared.namespaces,
		)
		.unwrap();
		assert!(matches!(route, Route::NewRequest(id) if id == request_id));
		shared.open_incoming(request_id, encode_msg(&msg, version)).unwrap();

		let (_, mut recv) = shared.incoming.pop().await.unwrap();

		// Drain the advertisement the stream opened with, so both halves are quiescent
		// and a later read only sees what the terminal message routes.
		assert_eq!(
			recv.read_chunk(usize::MAX).await.unwrap(),
			Some(encode_msg(&msg, version))
		);
		recv
	}

	/// Two relays meshed together, each advertising `namespace` to the other: ours on
	/// request 4, theirs on request 7. Returns their receive halves.
	///
	/// `last` is the advertisement registered second, which is the one a single map
	/// would leave as the sole entry. Each test passes the direction it must *not*
	/// match, so a direction-blind lookup lands on the wrong stream.
	async fn mesh(
		namespace: &str,
		version: Version,
		last: Direction,
	) -> (Arc<Shared>, VirtualRecvStream, VirtualRecvStream) {
		let shared = Arc::new(Shared::default());

		let (ours, theirs) = match last {
			Direction::Outgoing => {
				let theirs = receive(&shared, namespace, version).await;
				(advertise(&shared, namespace, version).await, theirs)
			}
			Direction::Incoming => {
				let ours = advertise(&shared, namespace, version).await;
				(ours, receive(&shared, namespace, version).await)
			}
		};

		(shared, ours, theirs)
	}

	/// Read until the stream FINs, returning whether it did so within `limit` reads.
	async fn drained(stream: &mut VirtualRecvStream, limit: usize) -> bool {
		for _ in 0..limit {
			if stream.read_chunk(usize::MAX).await.unwrap().is_none() {
				return true;
			}
		}
		false
	}

	#[test]
	fn test_namespace_reverse_lookup_v14() {
		let namespaces = Namespaces::default();
		namespaces.insert(Direction::Incoming, crate::Path::new("test/ns"), RequestId(42));

		let body = encode_body(
			&ietf::PublishNamespaceDone {
				track_namespace: crate::Path::new("test/ns"),
				request_id: RequestId(0),
			},
			Version::Draft14,
		);

		let route = classify(ietf::PublishNamespaceDone::ID, &body, Version::Draft14, &namespaces).unwrap();
		assert!(matches!(route, Route::CloseStream(RequestId(42))));
	}

	#[tokio::test]
	async fn test_publish_namespace_done_closes_only_inbound() {
		// The peer withdraws the advertisement it made, so its own stream closes and
		// ours keeps running.
		let version = Version::Draft14;
		let (shared, mut ours, mut theirs) = mesh("cluster/ns", version, Direction::Outgoing).await;

		let done = ietf::PublishNamespaceDone {
			track_namespace: crate::Path::new("cluster/ns"),
			request_id: RequestId(0),
		};
		let route = classify(
			ietf::PublishNamespaceDone::ID,
			&encode_body(&done, version),
			version,
			&shared.namespaces,
		)
		.unwrap();
		assert!(matches!(route, Route::CloseStream(RequestId(7))));

		shared.close(RequestId(7), encode_msg(&done, version));
		assert!(drained(&mut theirs, 4).await);
		assert!(shared.streams.lock().unwrap().contains_key(&RequestId(4)));
		assert!(ours.read_chunk(usize::MAX).now_or_never().is_none());
		// The withdrawn name is gone. Ours, travelling the other way, is still mapped.
		let namespace = crate::Path::new("cluster/ns");
		assert_eq!(shared.namespaces.get(Direction::Incoming, &namespace), None);
		assert_eq!(
			shared.namespaces.get(Direction::Outgoing, &namespace),
			Some(RequestId(4))
		);
	}

	#[tokio::test]
	async fn test_publish_namespace_cancel_closes_only_outbound() {
		// The peer rejects the advertisement we made, so ours closes and the one it
		// sent us keeps running.
		let version = Version::Draft14;
		let (shared, mut ours, mut theirs) = mesh("cluster/ns", version, Direction::Incoming).await;

		let cancel = ietf::PublishNamespaceCancel {
			track_namespace: crate::Path::new("cluster/ns"),
			request_id: RequestId(0),
			error_code: 0,
			reason_phrase: "".into(),
		};
		let route = classify(
			ietf::PublishNamespaceCancel::ID,
			&encode_body(&cancel, version),
			version,
			&shared.namespaces,
		)
		.unwrap();
		assert!(matches!(route, Route::CloseStream(RequestId(4))));

		shared.close(RequestId(4), encode_msg(&cancel, version));
		assert!(drained(&mut ours, 4).await);
		assert!(shared.streams.lock().unwrap().contains_key(&RequestId(7)));
		assert!(theirs.read_chunk(usize::MAX).now_or_never().is_none());
		// The rejected name is gone. The peer's advertisement of the same name stays.
		let namespace = crate::Path::new("cluster/ns");
		assert_eq!(shared.namespaces.get(Direction::Outgoing, &namespace), None);
		assert_eq!(
			shared.namespaces.get(Direction::Incoming, &namespace),
			Some(RequestId(7))
		);
	}

	/// A second announcement of a live name must not steal the withdrawal target.
	///
	/// The duplicate is delivered (the session may refuse it) and then dropped.
	/// PUBLISH_NAMESPACE_DONE still closes the first request, which is the one
	/// whose stream is still up.
	#[tokio::test]
	async fn test_duplicate_namespace_keeps_the_first_request() {
		let version = Version::Draft15;
		let shared = Arc::new(Shared::default());
		let namespace = crate::Path::new("twice");

		let mut first = receive_as(&shared, "twice", version, RequestId(1)).await;
		let second = receive_as(&shared, "twice", version, RequestId(3)).await;
		assert_eq!(
			shared.namespaces.get(Direction::Incoming, &namespace),
			Some(RequestId(1))
		);

		// The session refused the duplicate and dropped its stream.
		drop(second);
		assert_eq!(
			shared.namespaces.get(Direction::Incoming, &namespace),
			Some(RequestId(1))
		);

		let done = ietf::PublishNamespaceDone {
			track_namespace: namespace.clone(),
			request_id: RequestId(0),
		};
		let route = classify(
			ietf::PublishNamespaceDone::ID,
			&encode_body(&done, version),
			version,
			&shared.namespaces,
		)
		.unwrap();
		assert!(matches!(route, Route::CloseStream(RequestId(1))));

		shared.close(RequestId(1), encode_msg(&done, version));
		assert!(drained(&mut first, 4).await);
		assert_eq!(shared.namespaces.get(Direction::Incoming, &namespace), None);
	}

	/// A withdrawal for a name we never announced is not a control-stream error.
	/// The next real announcement still routes.
	#[test]
	fn test_unresolvable_withdrawal_is_ignored() {
		let version = Version::Draft15;
		let namespaces = Namespaces::default();

		let done = encode_body(
			&ietf::PublishNamespaceDone {
				track_namespace: crate::Path::new("ghost"),
				request_id: RequestId(0),
			},
			version,
		);
		let route = classify(ietf::PublishNamespaceDone::ID, &done, version, &namespaces).unwrap();
		assert!(matches!(route, Route::Ignore));

		let cancel = encode_body(
			&ietf::PublishNamespaceCancel {
				track_namespace: crate::Path::new("ghost"),
				request_id: RequestId(0),
				error_code: 0,
				reason_phrase: "".into(),
			},
			version,
		);
		let route = classify(ietf::PublishNamespaceCancel::ID, &cancel, version, &namespaces).unwrap();
		assert!(matches!(route, Route::Ignore));

		let announce = encode_body(&publish_namespace(RequestId(1), "real"), version);
		let route = classify(ietf::PublishNamespace::ID, &announce, version, &namespaces).unwrap();
		assert!(matches!(route, Route::NewRequest(RequestId(1))));
		assert_eq!(
			namespaces.get(Direction::Incoming, &crate::Path::new("real")),
			Some(RequestId(1))
		);
	}

	/// CANCEL frees the name, so the next announcement of it can be cancelled too.
	#[tokio::test]
	async fn test_cancel_releases_namespace_for_reannounce() {
		let version = Version::Draft14;
		let shared = Arc::new(Shared::default());
		let namespace = crate::Path::new("recycled");

		let mut first = advertise_as(&shared, "recycled", version, RequestId(0)).await;
		let cancel = ietf::PublishNamespaceCancel {
			track_namespace: namespace.clone(),
			request_id: RequestId(0),
			error_code: 0,
			reason_phrase: "".into(),
		};
		let route = classify(
			ietf::PublishNamespaceCancel::ID,
			&encode_body(&cancel, version),
			version,
			&shared.namespaces,
		)
		.unwrap();
		assert!(matches!(route, Route::CloseStream(RequestId(0))));
		shared.close(RequestId(0), encode_msg(&cancel, version));
		assert!(drained(&mut first, 4).await);
		assert_eq!(shared.namespaces.get(Direction::Outgoing, &namespace), None);

		let mut second = advertise_as(&shared, "recycled", version, RequestId(2)).await;
		let route = classify(
			ietf::PublishNamespaceCancel::ID,
			&encode_body(&cancel, version),
			version,
			&shared.namespaces,
		)
		.unwrap();
		assert!(matches!(route, Route::CloseStream(RequestId(2))));
		shared.close(RequestId(2), encode_msg(&cancel, version));
		assert!(drained(&mut second, 4).await);
		assert_eq!(shared.namespaces.get(Direction::Outgoing, &namespace), None);
	}

	/// Dropping the local reader, with no peer CANCEL, frees the name for reuse.
	#[tokio::test]
	async fn test_dropping_announcement_frees_namespace() {
		let version = Version::Draft15;
		let shared = Arc::new(Shared::default());
		let namespace = crate::Path::new("abandoned");

		let stream = advertise_as(&shared, "abandoned", version, RequestId(0)).await;
		assert_eq!(
			shared.namespaces.get(Direction::Outgoing, &namespace),
			Some(RequestId(0))
		);
		drop(stream);
		assert_eq!(shared.namespaces.get(Direction::Outgoing, &namespace), None);

		let stream = advertise_as(&shared, "abandoned", version, RequestId(2)).await;
		assert_eq!(
			shared.namespaces.get(Direction::Outgoing, &namespace),
			Some(RequestId(2))
		);
		drop(stream);
	}
}
