use core::{
    borrow::Borrow,
    fmt,
    future::{Future, IntoFuture},
    marker::PhantomData,
    ops::Deref,
    pin::Pin,
    ptr,
};

use abs_sync::cancellation::*;
use atomex::TrCmpxchOrderings;
use pincol::x_deps::atomic_sync::x_deps::{abs_sync, atomex};

use super::{
    oneshot_::{Oneshot, RecvFuture, WakerSlot},
    peeker_::Peeker,
    sender_::Sender,
};


#[derive(Debug)]
pub enum RxError<T> {
    /// Data is absent.
    ///
    /// This may occur only when a cancellation is raised during the await.
    Cancelled,

    /// The sender dropped before sending any data.
    Divorced(T),

    /// Data was received and thus moved.
    Drained(T),
}

impl<T> RxError<T> {
    pub fn rebind<U>(self, op: impl FnOnce(T) -> U) -> RxError<U> {
        match self {
            RxError::Cancelled => RxError::Cancelled,
            RxError::Divorced(x) => RxError::Divorced(op(x)),
            RxError::Drained(x) => RxError::Drained(op(x)),
        }
    }

    pub fn try_into_inner(self) -> Result<T, Self> {
        match self {
            RxError::Cancelled => Result::Err(self),
            RxError::Divorced(x) => Result::Ok(x),
            RxError::Drained(x) => Result::Ok(x),
        }
    }
}

impl<T: fmt::Display> fmt::Display for RxError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RxError::Divorced(t) => write!(f, "ReceiverError::Divoced({t})"),
            RxError::Drained(t) => write!(f, "ReceiverError::Drained({t})"),
            RxError::Cancelled => write!(f, "ReceiverError::Cancelled"),
        }
    }
}

/// The unique receiver of the `Oneshot` channel that can receive the only
/// message once, before being converted to a `Peeker` which can clone and view
/// the message reference.
pub struct Receiver<B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    oneshot_: Option<B>,
    wake_slot_: WakerSlot<O>,
    _unused_t_: PhantomData<Oneshot<T, O>>,
}

impl<B, T, O> Receiver<B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    pub(super) fn new(oneshot: B) -> Self {
        Receiver {
            oneshot_: Option::Some(oneshot),
            wake_slot_: WakerSlot::new(Option::None),
            _unused_t_: PhantomData,
        }
    }

    #[inline]
    pub(super) fn oneshot(&self) -> &Oneshot<T, O> {
        let Option::Some(oneshot) = &self.oneshot_ else {
            unreachable!("[Receiver::oneshot] already taken.")
        };
        oneshot.borrow()
    }

    #[inline]
    pub(super) fn waker_slot(self: Pin<&mut Self>) -> Pin<&mut WakerSlot<O>> {
        unsafe {
            let this = self.get_unchecked_mut();
            Pin::new_unchecked(&mut this.wake_slot_)
        }
    }

    pub fn try_into(mut self) -> Result<Peeker<B, T, O>, RxError<()>> {
        self.oneshot_
            .take()
            .map(Peeker::new)
            .ok_or(RxError::Divorced(()))
    }

    #[inline]
    pub fn is_paired_with(&self, sender: &Sender<B, T, O>) -> bool {
        ptr::eq(self.oneshot(), sender.oneshot())
    }

    #[inline]
    pub fn is_data_ready(&self) -> bool {
        self.oneshot().is_data_ready()
    }

    #[inline]
    pub fn can_receive(&self) -> bool {
        self.oneshot().can_receive()
    }
}

impl<B, T, O> Receiver<B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    #[inline(always)]
    pub fn receive_async(self: Pin<&mut Self>) -> ReceiveAsync<'_, B, T, O> {
        ReceiveAsync::new(self)
    }
}

impl<B, T, O> Drop for Receiver<B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        let Option::Some(p) = self.oneshot_.take() else {
            return;
        };
        // The receiver may have closed the rx flag during receiving.
        let _ = p.borrow().try_set_rx_closed();
    }
}

pub struct ReceiveAsync<'a, B, T, O>(Pin<&'a mut Receiver<B, T, O>>)
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings;

impl<'a, B, T, O> ReceiveAsync<'a, B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    pub(super) fn new(receiver: Pin<&'a mut Receiver<B, T, O>>) -> Self {
        ReceiveAsync(receiver)
    }

    pub fn may_cancel_with<'f, C>(
        self,
        cancel: Pin<&'f mut C>,
    ) -> RecvFuture<'a, 'f, C, B, T, O>
    where
        C: TrCancellationToken,
    {
        RecvFuture::new(self.0, cancel)
    }
}

impl<'a, B, T, O> IntoFuture for ReceiveAsync<'a, B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    type IntoFuture = RecvFuture<'a, 'a, NonCancellableToken, B, T, O>;
    type Output = <Self::IntoFuture as Future>::Output;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        RecvFuture::new(self.0, cancel)
    }
}

impl<B, T, O> TrIntoFutureMayCancel for ReceiveAsync<'_, B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    type MayCancelOutput = <Self as IntoFuture>::Output;

    #[inline(always)]
    fn may_cancel_with<'f, C: TrCancellationToken>(
        self,
        cancel: Pin<&'f mut C>,
    ) -> impl Future<Output = Self::MayCancelOutput>
    where
        Self: 'f,
    {
        ReceiveAsync::may_cancel_with(self, cancel)
    }
}
