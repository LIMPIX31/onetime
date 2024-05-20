use std::{
	cell::UnsafeCell,
	marker::PhantomPinned,
	mem::MaybeUninit,
	pin::Pin,
	ptr,
	sync::{
		atomic::{AtomicBool, Ordering},
		Arc,
	},
	task::{ready, Poll},
};

use event_listener_strategy::{
	easy_wrapper,
	event_listener::{Event, EventListener},
	EventListenerFuture, Strategy,
};
use pin_project_lite::pin_project;
use thiserror::Error;

#[derive(Debug)]
struct Channel<T> {
	slot: Slot<T>,
	send: Event,
	closed: AtomicBool,
	has_receiver: AtomicBool,
}

#[derive(Debug)]
struct Slot<T> {
	value: UnsafeCell<MaybeUninit<T>>,
	sent: AtomicBool,
}

impl<T> Slot<T> {
	fn put(&self, val: T) {
		let slot = self.value.get().cast::<T>();
		unsafe {
			ptr::write(slot, val);
		}
		self.sent.store(true, Ordering::Relaxed);
	}

	fn is_sent(&self) -> bool {
		self.sent.load(Ordering::Relaxed)
	}

	unsafe fn value(&self) -> T {
		let slot = self.value.get();
		let mut value = MaybeUninit::<T>::uninit();
		ptr::swap(slot, ptr::from_mut(&mut value));
		value.assume_init()
	}
}

impl<T> Default for Slot<T> {
	fn default() -> Self {
		Slot { value: UnsafeCell::new(MaybeUninit::uninit()), sent: AtomicBool::new(false) }
	}
}

unsafe impl<T: Send> Send for Slot<T> {}
unsafe impl<T: Send> Sync for Slot<T> {}

#[derive(Debug)]
pub struct Sender<T> {
	channel: Arc<Channel<T>>,
	sent: bool,
}

impl<T> Sender<T> {
	/// Once sent, the channel will be closed and the sent value will be waiting
	/// to be consumed on the [Receiver]
	///
	/// # Errors
	/// In case the Receiver has been dropped
	pub fn send(mut self, msg: T) -> Result<(), SendError<T>> {
		if self.channel.has_receiver.load(Ordering::Relaxed) {
			self.sent = true;
			self.channel.slot.put(msg);
			self.channel.send.notify(1);
			Ok(())
		} else {
			Err(SendError(self, msg))
		}
	}
}

impl<T> Drop for Sender<T> {
	fn drop(&mut self) {
		if !self.sent {
			self.channel.closed.store(true, Ordering::Relaxed);
		}
	}
}

/// Stores the [Sender] and the value being sent for reuse after a failed send
#[derive(Debug, Error)]
#[error("sending into closed channel")]
pub struct SendError<T>(Sender<T>, T);

impl<T> SendError<T> {
	pub fn into_inner(self) -> (Sender<T>, T) {
		(self.0, self.1)
	}
}

#[derive(Debug)]
pub struct Receiver<T> {
	channel: Arc<Channel<T>>,
}

impl<T> Receiver<T> {
	/// Tries to receive value from the channel
	///
	/// # Errors
	/// In case [Sender] has been dropped or the value has not yet been sent
	pub fn try_recv(self) -> Result<T, TryRecvError<T>> {
		if self.channel.slot.is_sent() {
			// # Safety:
			// Value is sent
			let value = unsafe { self.channel.slot.value() };

			Ok(value)
		} else if self.channel.closed.load(Ordering::Relaxed) {
			Err(TryRecvError::Closed)
		} else {
			Err(TryRecvError::Empty(self))
		}
	}

	/// Receives value from the channel
	/// Waits for the value to appear in the slot
	pub fn recv(self) -> Recv<T> {
		Recv::_new(RecvInner { receiver: Some(self), listener: None, pin: PhantomPinned })
	}

	/// Receives value from the channel (blocking way)
	/// Waits for the value to appear in the slot
	///
	/// # Errors
	/// In case [Sender] has been dropped
	#[cfg(all(feature = "std", not(target_family = "wasm")))]
	pub fn recv_blocking(self) -> Result<T, RecvError> {
		self.recv().wait()
	}
}

impl<T> Drop for Receiver<T> {
	fn drop(&mut self) {
		self.channel.has_receiver.store(false, Ordering::Relaxed);
	}
}

/// The sender was dropped, or the value has not yet been sent
#[derive(Debug, Error)]
pub enum TryRecvError<T> {
	#[error("receiving from an empty channel")]
	Empty(Receiver<T>),
	#[error("receiving from an empty and closed channel")]
	Closed,
}

easy_wrapper! {
		/// A future returned by [`Receiver::recv()`].
		#[derive(Debug)]
		#[must_use = "futures do nothing unless you `.await` or poll them"]
		pub struct Recv<T>(RecvInner<T> => Result<T, RecvError>);
		#[cfg(all(feature = "std", not(target_family = "wasm")))]
		pub(crate) wait();
}

pin_project! {
	#[derive(Debug)]
	#[project(!Unpin)]
	struct RecvInner<T> {
		receiver: Option<Receiver<T>>,

		listener: Option<EventListener>,

		#[pin]
		pin: PhantomPinned
	}
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Error)]
#[error("receiving from a closed channel")]
pub struct RecvError;

impl<T> EventListenerFuture for RecvInner<T> {
	type Output = Result<T, RecvError>;

	fn poll_with_strategy<'a, S: Strategy<'a>>(
		self: Pin<&mut Self>,
		strategy: &mut S,
		context: &mut S::Context,
	) -> Poll<Self::Output> {
		let this = self.project();

		loop {
			let Some(receiver) = this.receiver.take() else { unreachable!() };

			let channel = receiver.channel.clone();

			match receiver.try_recv() {
				Ok(msg) => return Poll::Ready(Ok(msg)),
				Err(TryRecvError::Closed) => return Poll::Ready(Err(RecvError)),
				Err(TryRecvError::Empty(receiver)) => *this.receiver = Some(receiver),
			}

			if this.listener.is_some() {
				ready!(S::poll(strategy, &mut *this.listener, context));
			} else {
				*this.listener = Some(channel.send.listen());
			}
		}
	}
}

/// Creates onetime channel
#[must_use]
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
	let channel = Arc::new(Channel::<T> {
		slot: Slot::default(),
		send: Event::new(),
		closed: AtomicBool::new(true),
		has_receiver: AtomicBool::new(true),
	});

	let s = Sender { channel: channel.clone(), sent: false };
	let r = Receiver { channel: channel.clone() };

	(s, r)
}

#[cfg(test)]
mod tests {
	use futures_lite::future;

	use super::*;

	#[allow(unused)]
	fn assert_bounds() {
		fn is_send<T: Send>() {}
		fn is_sync<T: Sync>() {}

		is_send::<Sender<()>>();
		is_sync::<Sender<()>>();
		is_send::<Receiver<()>>();
		is_sync::<Receiver<()>>();
	}

	#[test]
	fn send() {
		let (tx, rx) = channel();

		assert!(tx.send("ok").is_ok());
		assert!(matches!(future::block_on(rx.recv()), Ok("ok")));
	}

	#[test]
	fn drop_receiver() {
		let (tx, rx) = channel();

		drop(rx);

		assert!(tx.send("ok").is_err());
	}

	#[test]
	fn drop_sender() {
		let (tx, rx) = channel::<()>();

		drop(tx);

		assert!(rx.recv_blocking().is_err());
	}

	#[test]
	fn recv_blocking() {
		let (tx, rx) = channel();

		assert!(tx.send("ok").is_ok());
		assert!(matches!(rx.recv_blocking(), Ok("ok")));
	}

	#[test]
	fn send_from_task() {
		smol::block_on(async move {
			let (tx, rx) = channel();

			smol::spawn(async move {
				assert!(tx.send(42).is_ok());
			})
			.detach();

			smol::spawn(async move {
				assert!(matches!(rx.recv().await, Ok(42)));
			})
			.detach();
		});
	}

	#[test]
	fn hundreed_of_channels() {
		smol::block_on(async move {
			let mut rxs = Vec::new();

			for i in 0..128_usize {
				let (tx, rx) = channel();
				rxs.push(rx);

				smol::spawn(async move {
					assert!(tx.send(i).is_ok());
				})
				.detach();
			}

			for (idx, rx) in rxs.into_iter().enumerate() {
				smol::spawn(async move {
					let result = rx.recv().await;
					assert!(result.is_ok());
					if let Ok(result) = result {
						assert_eq!(result, idx);
					}
				})
				.detach();
			}
		});
	}
}
