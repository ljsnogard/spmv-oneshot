use core::{
    borrow::Borrow,
    cmp::{PartialEq, Eq},
    future::{Future, IntoFuture},
    marker::PhantomData,
    ops::Deref,
    pin::Pin,
    ptr::{self, NonNull},
    task::{Context, Poll},
};

use pin_utils::pin_mut;
use pin_project::pin_project;

use abs_sync::cancellation::{
    NonCancellableToken, TrCancellationToken, TrIntoFutureMayCancel,
};
use atomex::TrCmpxchOrderings;
use pincol::x_deps::atomic_sync::x_deps::{abs_sync, atomex};

use super::{
    oneshot_::{Oneshot, WakerSlot},
    recver_::RxError,
    sender_::Sender,
};

/// A clonable viewer of the received message.
#[derive(Debug)]
pub struct Peeker<B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    oneshot_: B,
    wake_slot_: WakerSlot<O>,
    _unused_t_: PhantomData<Oneshot<T, O>>,
}

impl<B, T, O> Peeker<B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    /// Creates a peeker and increase the peeker count in SharedCore.
    pub(super) fn new(oneshot: B) -> Self {
        oneshot.borrow().increase_peeker_count();
        Peeker {
            oneshot_: oneshot,
            wake_slot_: WakerSlot::new(Option::None),
            _unused_t_: PhantomData,
        }
    }

    #[inline(always)]
    pub fn can_peek(&self) -> bool {
        self.oneshot_.can_peek()
    }

    #[inline(always)]
    pub fn is_data_ready(&self) -> bool {
        self.oneshot_.is_data_ready()
    }

    pub fn try_peek(&self) -> Result<Option<&T>, RxError<()>> {
        self.oneshot_.try_peek()
    }

    pub fn peek_async(mut self: Pin<&mut Self>) -> PeekAsync<'_, B, T, O> {
        let slot = self.as_mut().wake_slot();
        let _ = slot.data_pinned().take();
        PeekAsync::new(self)
    }

    pub fn alias_count(&self) -> usize {
        self.oneshot_.peeker_count()
    }

    pub fn is_paired_with(&self, sender: &Sender<B, T, O>) -> bool {
        ptr::eq(self.oneshot_.deref(), sender.oneshot())
    }

    fn wake_slot(self: Pin<&mut Self>) -> Pin<&mut WakerSlot<O>> {
        unsafe {
            let pointer = &mut self.get_unchecked_mut().wake_slot_;
            Pin::new_unchecked(pointer)
        }
    }
}

impl<B, T, O> Clone for Peeker<B, T, O>
where
    B: Deref<Target = Oneshot<T, O>> + Clone,
    O: TrCmpxchOrderings,
{
    fn clone(&self) -> Self {
        Self::new(self.oneshot_.clone())
    }
}

impl<B, T, O> Drop for Peeker<B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        self.oneshot_.borrow().decrease_peeker_count();
    }
}

impl<B, T, O> PartialEq for Peeker<B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self.oneshot_.borrow(), other.oneshot_.borrow())
    }
}

impl<B, T, O> Eq for Peeker<B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{}

pub struct PeekAsync<'a, B, T, O>(Pin<&'a mut Peeker<B, T, O>>)
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings;

impl<'a, B, T, O> PeekAsync<'a, B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    pub fn new(peeker: Pin<&'a mut Peeker<B, T, O>>) -> Self {
        PeekAsync(peeker)
    }

    pub fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> PeekFuture<'a, C, B, T, O>
    where
        C: TrCancellationToken,
    {
        PeekFuture::new(self.0, cancel)
    }
}

impl<'a, B, T, O> IntoFuture for PeekAsync<'a, B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    type IntoFuture = PeekFuture<'a, NonCancellableToken, B, T, O>;
    type Output = <Self::IntoFuture as Future>::Output;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        PeekFuture::new(self.0, cancel)
    }
}

impl<'a, B, T, O> TrIntoFutureMayCancel<'a> for PeekAsync<'a, B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    type MayCancelOutput = Result<&'a T, RxError<()>>;

    #[inline(always)]
    fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> impl Future<Output = Self::MayCancelOutput>
    where
        C: TrCancellationToken,
    {
        PeekAsync::may_cancel_with(self, cancel)
    }
}

#[pin_project]
pub struct PeekFuture<'a, C, B, T, O>
where
    C: TrCancellationToken,
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    peeker_: Pin<&'a mut Peeker<B, T, O>>,
    cancel_: Pin<&'a mut C>,
}

impl<'a, C, B, T, O> PeekFuture<'a, C, B, T, O>
where
    C: TrCancellationToken,
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    pub(super) fn new(
        peeker: Pin<&'a mut Peeker<B, T, O>>,
        cancel: Pin<&'a mut C>,
    ) -> Self {
        PeekFuture {
            peeker_: peeker,
            cancel_: cancel,
        }
    }
}

impl<'a, C, B, T, O> Future for PeekFuture<'a, C, B, T, O>
where
    C: TrCancellationToken,
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    type Output = Result<&'a T, RxError<()>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let peeker = this.peeker_.as_mut();
        let oneshot = unsafe {
            let p: &Oneshot<_, _> = peeker.oneshot_.borrow();
            let p = p as *const _ as *mut Oneshot<T, O>;
            let p = NonNull::new_unchecked(p);
            p.as_ref()
        };
        let mut cancel = this.cancel_.as_mut();
        let try_peek = oneshot.try_peek();

        let Result::Ok(opt) = try_peek else {
            let Result::Err(e) = try_peek else {
                unreachable!("[PeekFuture::poll] oneshot.try_peek")
            };
            #[cfg(test)]
            log::trace!("[PeekFuture::poll]({oneshot:p}) try_peek err({e:?})");
            return Poll::Ready(Result::Err(e));
        };
        if let Option::Some(t) = opt {
            #[cfg(test)]
            log::trace!("[PeekFuture::poll]({oneshot:p}) try_peek ok");
            return Poll::Ready(Result::Ok(t));
        }
        let peeker_pin = this.peeker_.as_mut();
        let mut slot = peeker_pin.wake_slot();
        if slot.data().is_none() {
            {
                let fut_cancel = cancel.as_mut().cancellation().into_future();
                pin_mut!(fut_cancel);
                if fut_cancel.poll(cx).is_ready() {
                    #[cfg(test)]
                    log::trace!("[PeekFuture::poll]({oneshot:p}) Cancelled 1");
                    return Poll::Ready(Result::Err(RxError::Cancelled));
                }
            }
            let mutex = oneshot.wake_queue().mutex();
            let acq = mutex.acquire();
            pin_mut!(acq);
            let opt_g = acq.lock().may_cancel_with(cancel);
            let Option::Some(mut g) = opt_g else {
                #[cfg(test)]
                log::trace!("[PeekFuture::poll]({oneshot:p}) Cancelled 2");
                return Poll::Ready(Result::Err(RxError::Cancelled));
            };
            debug_assert!(slot.attached_list().is_none());

            slot.as_mut()
                .data_pinned()
                .get_mut()
                .replace(cx.waker().clone());
            let queue_pin = (*g).as_mut();
            let r = queue_pin.push_tail(slot.as_mut());
            debug_assert!(r.is_ok());
        };
        #[cfg(test)]
        log::trace!("[PeekFuture::poll]({oneshot:p}) pending");
        Poll::Pending
    }
}
