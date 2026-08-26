//! Zero-cost `Sync` wrapper for types that are `Send` but not `Sync`.
//!
//! [`ExclusiveSync`] makes any `Send` type `Sync` by wrapping it in a mutex
//! that is **never locked during exclusive access**:
//!
//! 1. Rust's borrow checker guarantees `&mut`/`Pin<&mut Self>` access is
//!    exclusive, so `poll`/`poll_next`-style methods bypass the mutex.
//! 2. Only shared `&self` access (rare on poll-driven types) takes the lock,
//!    via [`ExclusiveSync::with`].
//!
//! This bridges `!Sync` poll-driven types (hyper/axum bodies, response
//! streams, service futures) into contexts whose bounds require `Sync`
//! (tower service stacks, `Arc`-shared state) without paying a lock on the
//! hot poll path. Same justification as `sync_wrapper::SyncWrapper`.
//!
//! Trait forwarding for [`Future`] and [`Stream`] is provided here; protocol
//! traits owned by transport crates (`http_body::Body`, `tower::Service`)
//! forward at the consumer through [`ExclusiveSync::get_pin_mut`] /
//! [`ExclusiveSync::get_mut`] for exclusive methods and
//! [`ExclusiveSync::with`] for shared ones.

use std::{
	pin::Pin,
	task::{Context, Poll},
};

use futures_core::Stream;
use parking_lot::Mutex;

/// Zero-cost `Sync` wrapper for `Send` types.
///
/// Exclusive access (`&mut self`, `Pin<&mut Self>`) never locks; shared
/// `&self` access goes through [`ExclusiveSync::with`], which does.
///
/// # Example
///
/// ```
/// use std::cell::Cell;
///
/// use omp_core::ExclusiveSync;
///
/// fn assert_sync<T: Sync>(_: &T) {}
///
/// // Cell is Send but !Sync; the wrapper restores Sync.
/// let wrapped = ExclusiveSync::new(Cell::new(1));
/// assert_sync(&wrapped);
/// assert_eq!(wrapped.into_inner().get(), 1);
/// ```
#[derive(Debug)]
#[repr(transparent)]
pub struct ExclusiveSync<B>(Mutex<B>);

impl<B> ExclusiveSync<B> {
	/// Wraps a value to make it `Sync`.
	pub const fn new(inner: B) -> Self {
		Self(Mutex::new(inner))
	}

	/// Unwraps the inner value.
	#[inline]
	pub fn into_inner(self) -> B {
		self.0.into_inner()
	}

	/// Returns a pinned mutable reference without locking.
	///
	/// `Pin<&mut Self>` already guarantees exclusive access via the borrow
	/// checker; no other thread can reach the inner value while this
	/// reference exists.
	#[inline]
	pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut B> {
		// SAFETY: structural pinning projection — if `Self` is pinned, the inner
		// `B` stays pinned: it is never moved out of the mutex through a pinned
		// reference, and `Mutex` has no `Drop` that moves it.
		unsafe { Pin::new_unchecked(self.get_unchecked_mut().0.get_mut()) }
	}

	/// Returns a mutable reference without locking.
	///
	/// `&mut self` guarantees exclusive access.
	#[inline]
	pub fn get_mut(&mut self) -> &mut B {
		self.0.get_mut()
	}

	/// Runs `f` with shared access to the inner value, taking the lock.
	///
	/// This is the only access path that locks; use it to forward shared
	/// `&self` trait methods (e.g. `Body::is_end_stream`, `size_hint`).
	#[inline]
	pub fn with<R>(&self, f: impl FnOnce(&B) -> R) -> R {
		f(&self.0.lock())
	}
}

impl<B: Clone> Clone for ExclusiveSync<B> {
	fn clone(&self) -> Self {
		Self(Mutex::new(self.0.lock().clone()))
	}
}

// SAFETY: exclusive access (`&mut`, `Pin<&mut>`) never locks and is already
// exclusive via the borrow checker; shared access (`&self`) always locks the
// mutex; `B: Send` allows the inner value to move between threads. Same
// safety argument as `sync_wrapper::SyncWrapper`.
unsafe impl<B> Sync for ExclusiveSync<B> where B: Send {}

impl<F: Future> Future for ExclusiveSync<F> {
	type Output = F::Output;

	// Exclusive access via Pin<&mut Self> — no lock.
	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		self.get_pin_mut().poll(cx)
	}
}

impl<S: Stream> Stream for ExclusiveSync<S> {
	type Item = S::Item;

	// Exclusive access via Pin<&mut Self> — no lock.
	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		self.get_pin_mut().poll_next(cx)
	}

	fn size_hint(&self) -> (usize, Option<usize>) {
		self.with(Stream::size_hint)
	}
}

#[cfg(test)]
mod tests {
	use std::{cell::Cell, task::Waker};

	use super::*;

	// Compile-time proof: wrapping a Send + !Sync type yields Sync.
	const fn require_sync<T: Sync>() {}
	const _: () = require_sync::<ExclusiveSync<Cell<u8>>>();

	#[test]
	fn future_and_stream_forward_through_pin() {
		let mut cx = Context::from_waker(Waker::noop());

		let mut fut = ExclusiveSync::new(std::future::ready(7));
		assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Ready(7));

		let mut stream = ExclusiveSync::new(futures::stream::iter([1, 2]));
		assert_eq!(Pin::new(&mut stream).poll_next(&mut cx), Poll::Ready(Some(1)));
		assert_eq!(stream.size_hint(), (1, Some(1)));
		assert_eq!(Pin::new(&mut stream).poll_next(&mut cx), Poll::Ready(Some(2)));
		assert_eq!(Pin::new(&mut stream).poll_next(&mut cx), Poll::Ready(None));
	}

	#[test]
	fn shared_access_locks_exclusive_access_mutates() {
		let mut wrapped = ExclusiveSync::new(Cell::new(1));
		wrapped.get_mut().set(2);
		assert_eq!(wrapped.with(Cell::get), 2);
		assert_eq!(wrapped.into_inner().get(), 2);
	}
}
