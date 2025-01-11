use core::{
    cell::UnsafeCell,
    fmt,
    future::{Future, IntoFuture},
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr::NonNull,
    sync::atomic::*,
    task::{Context, Poll, Waker},
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_sync::cancellation::{CancelledToken, TrCancellationToken};
use atomex::{AtomicFlags, StrictOrderings, TrAtomicData, TrCmpxchOrderings};
use atomic_sync::{
    mutex::preemptive::{SpinningMutex, TrMutexSignal},
    x_deps::abs_sync,
};
use pincol::{
    linked_list::{PinnedList, PinnedSlot},
    x_deps::{atomex, atomic_sync},
};

use super::{
    peeker_::Peeker,
    recver_::{Receiver, RxError},
    sender_::{SendTask, Sender, TxError},
};

pub(super) type WakeQueue<O> = PinnedList<Option<Waker>, O>;
pub(super) type WakerSlot<O> = PinnedSlot<Option<Waker>, O>;

/// `Oneshot` channel is capable and capable of sending and 
/// receiving a single message only once.
/// 
/// It can also be split into a pair of tx and rx.
#[derive(Debug)]
pub struct Oneshot<T, O = StrictOrderings>
where
    O: TrCmpxchOrderings,
{
    flags_: Flags<O>,
    queue_: WakeQueue<O>,
    value_: UnsafeCell<MaybeUninit<T>>,
}

impl<T, O> Oneshot<T, O>
where
    O: TrCmpxchOrderings,
{
    pub const fn new() -> Self {
        Oneshot {
            flags_: Flags::new(AtomicUsize::new(0)),
            queue_: WakeQueue::new(),
            value_: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

type Split<S, T, O> = (Sender<S, T, O>, Receiver<S, T, O>);

impl<T, O> Oneshot<T, O>
where
    T: Send + Sync,
    O: TrCmpxchOrderings,
{
    /// Split the channel into a sender and a receiver
    /// 
    /// # Example
    /// ```
    /// use pin_utils::pin_mut;
    /// 
    /// use atomex::StrictOrderings;
    /// use spmv_oneshot::{x_deps::atomex, Oneshot};
    ///
    /// let oneshot = Oneshot::<(), StrictOrderings>::new();
    /// pin_mut!(oneshot);
    /// let (tx, rx) = Oneshot::split(oneshot);
    /// ```
    pub fn split(self: Pin<&mut Self>) -> Split<Pin<&Self>, T, O> {
        let p = unsafe {
            NonNull::new_unchecked(self.get_unchecked_mut())
        };
        let sender = Sender::new(unsafe {
            Pin::new_unchecked(p.as_ref())
        });
        let recver = Receiver::new(unsafe {
            Pin::new_unchecked(p.as_ref())
        });
        (sender, recver)
    }

    pub fn try_split<S>(
        oneshot: S,
        strong_count: impl FnOnce(&S) -> usize,
        weak_count: impl FnOnce(&S) -> usize,
    ) -> Result<Split<S, T, O>, S>
    where
        S: Clone + Send + Sync + Deref<Target = Self>,
    {
        let x = strong_count(&oneshot) > 1 ||
            weak_count(&oneshot) > 0;
        if x {
            Result::Err(oneshot)
        } else {
            let sender = Sender::new(oneshot.clone());
            let recver = Receiver::new(oneshot);
            Result::Ok((sender, recver))
        }
    }
}

impl<T, O> Oneshot<T, O>
where
    O: TrCmpxchOrderings,
{
    /// Tell whether the tx end has sent data.
    #[inline(always)]
    pub fn is_triggered(&self) -> bool {
        self.flags_.has_tx_sent()
    }

    /// Tell whether the tx end has sent data AND rx end has NOT yet received.
    ///
    /// Note: peeking will not change the data readiness.
    #[inline(always)]
    pub fn is_data_ready(&self) -> bool {
        self.flags_.is_data_ready()
    }

    /// Oneshot can_send will only return true when all the followings are met:
    /// * Never `try_send` or `send``;
    /// * The `Receiver` or at least one `Peeker` is alive;
    #[inline(always)]
    pub fn can_send(&self) -> bool {
        self.flags_.can_send()
    }

    #[inline(always)]
    pub fn can_receive(&self) -> bool {
        self.flags_.can_receive()
    }

    #[inline(always)]
    pub fn can_peek(&self) -> bool {
        self.flags_.can_peek()
    }

    #[inline(always)]
    pub fn send(&self, data: T) -> SendTask<'_, T, O> {
        SendTask::new(self, data)
    }

    pub fn try_send(&self, data: T) -> Result<(), TxError<T>> {
        let cancel = CancelledToken::pinned();
        self.send_with_cancel_(data, cancel)
    }

    pub fn try_peek(&self) -> Result<Option<&T>, RxError<()>> {
        let s = self.flags_.value();
        if Flags::<O>::expect_data_valid(s) {
            let mutex = self.value_mutex();
            let acq = mutex.acquire();
            pin_mut!(acq);
            let g = acq.lock().wait();
            let t = unsafe {
                let p = (*g).as_ptr();
                &*p
            };
            return Result::Ok(Option::Some(t));
        }
        if self.flags_.is_tx_closed() {
            Result::Err(RxError::Divorced(()))
        } else {
            Result::Ok(Option::None)
        }
    }

    pub fn peeker(&self) -> Peeker<&Self, T, O> {
        Peeker::new(self)
    }
}

impl<T, O> Oneshot<T, O>
where
    O: TrCmpxchOrderings,
{
    pub(super) fn value_mutex(&self)
        -> SpinningMutex<
            &mut MaybeUninit<T>,
            usize,
            &mut AtomicUsize,
            Flags<O>,
            O,
        >
    {
        let data = unsafe {
            let mut p = NonNull::new_unchecked(self.value_.get());
            p.as_mut()
        };
        let cell = unsafe {
            let p = &self.flags_.0.as_ref() as *const _ as *mut AtomicUsize;
            let mut p = NonNull::new_unchecked(p);
            p.as_mut()
        };
        SpinningMutex::new(data, cell)
    }

    pub(super) fn send_with_cancel_<'a, C>(
        &'a self,
        data: T,
        cancel: Pin<&'a mut C>,
    ) -> Result<(), TxError<T>>
    where
        C: TrCancellationToken,
    {
        let mutex = self.value_mutex();
        let acq = mutex.acquire();
        pin_mut!(acq);
        let lock_res = acq.lock().may_cancel_with(cancel);
        let Option::Some(mut guard) = lock_res else {
            return Result::Err(TxError::Cancelled);
        };
        if let Result::Err(s) = self.flags_.try_set_sent() {
            let e = if Flags::<O>::expect_data_valid(s)
                || Flags::<O>::expect_tx_expire(s)
            {
                TxError::Stuffed(data)
            } else if Flags::<O>::expect_rx_closed(s) {
                TxError::Divorced(data)
            } else {
                unreachable!("[SendTask::may_cancel_with] {:?}", &self.flags_);
            };
            return Result::Err(e);
        };
        unsafe {
            guard.deref_mut()
                .as_mut_ptr()
                .write(data)
        };
        drop(guard);
        self.wake_all();
        Result::Ok(())
    }

    pub(super) fn wake_all(&self) {
        fn wake_<Ord: TrCmpxchOrderings>(
            slot: Pin<&mut WakerSlot<Ord>>,
        ) -> bool{
            let Option::Some(waker) = slot.data_pinned().take() else {
                return true;
            };
            waker.wake();
            true
        }

        let mutex = self.queue_.mutex();
        let acq = mutex.acquire();
        pin_mut!(acq);
        let mut g = acq.lock().wait();
        let queue_pin = (*g).as_mut();
        let _ = queue_pin.clear(wake_);
    }

    pub(super) fn try_set_tx_closed(&self) -> Result<Uval, Uval> {
        let r = self.flags_.try_set_tx_closed();
        if r.is_ok() {
            self.wake_all();
        }
        r
    }

    pub(super) fn try_set_rx_closed(&self) -> Result<Uval, Uval> {
        self.flags_.try_set_rx_closed()
    }

    #[inline(always)]
    pub(super) fn increase_peeker_count(&self) -> usize {
        self.flags_.increase_peeker_count()
    }

    #[inline(always)]
    pub(super) fn decrease_peeker_count(&self) -> usize {
        self.flags_.decrease_peeker_count()
    }

    #[inline(always)]
    pub(crate) fn peeker_count(&self) -> usize {
        self.flags_.peeker_count()
    }

    #[inline(always)]
    pub(super) fn try_set_data_valid(
        &self,
        is_valid: bool,
    ) -> Result<Uval, Uval> {
        self.flags_.try_set_data_valid(is_valid)
    }

    #[inline(always)]
    pub(super) fn wake_queue(&self) -> &WakeQueue<O> {
        &self.queue_
    }
}

impl<T, O> Default for Oneshot<T, O>
where
    O: TrCmpxchOrderings,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T, O> Drop for Oneshot<T, O>
where
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        let _ = self.try_set_tx_closed();
        let _ = self.try_set_rx_closed();
        if !self.flags_.is_data_ready() {
            return;
        }
        unsafe {
            self.value_
                .get_mut()
                .as_mut_ptr()
                .drop_in_place()
        };
    }
}

unsafe impl<T, O> Send for Oneshot<T, O>
where
    T: Send,
    O: TrCmpxchOrderings,
{}

unsafe impl<T, O> Sync for Oneshot<T, O>
where
    T: Send,
    O: TrCmpxchOrderings,
{}

#[pin_project]
pub struct RecvFuture<'ctx, 'tok, C, B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    C: TrCancellationToken,
    O: TrCmpxchOrderings,
{
    recver_: Pin<&'ctx mut Receiver<B, T, O>>,
    cancel_: Pin<&'tok mut C>,
}

impl<'ctx, 'tok, C, B, T, O> RecvFuture<'ctx, 'tok, C, B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    C: TrCancellationToken,
    O: TrCmpxchOrderings,
{
    pub(super) fn new(
        receiver: Pin<&'ctx mut Receiver<B, T, O>>,
        cancel: Pin<&'tok mut C>,
    ) -> Self {
        RecvFuture {
            recver_: receiver,
            cancel_: cancel,
        }
    }
}

impl<C, B, T, O> Future for RecvFuture<'_, '_, C, B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    C: TrCancellationToken,
    O: TrCmpxchOrderings,
{
    type Output = Result<T, RxError<()>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let oneshot = unsafe {
            let recver_ptr = this.recver_.as_mut().get_unchecked_mut();
            let p = NonNull::new_unchecked(recver_ptr);
            p.as_ref().oneshot()
        };
        let mut slot = this.recver_.as_mut().waker_slot();
        let mut cancel = this.cancel_.as_mut();

        let s = oneshot.flags_.value();
        let data_invalid = Flags::<O>::expect_data_invalid(s);
        let tx_open = Flags::<O>::expect_tx_open(s);

        if data_invalid && cancel.is_cancelled() {
            #[cfg(test)]
            log::trace!("[RecvFuture::Poll] Cancelled 1");
            return Poll::Ready(Result::Err(RxError::Cancelled));
        }
        if data_invalid && tx_open {
            if slot.data().is_none() {
                {
                    let f = cancel.as_mut().cancellation().into_future();
                    pin_mut!(f);
                    if f.poll(cx).is_ready() {
                        #[cfg(test)]
                        log::trace!("[RecvFuture::Poll] Cancelled 2");
                        return Poll::Ready(Result::Err(RxError::Cancelled));
                    }
                }
                let mutex = oneshot.wake_queue().mutex();
                let acq = mutex.acquire();
                pin_mut!(acq);
                let opt_g = acq.lock().may_cancel_with(cancel);
                let Option::Some(mut g) = opt_g else {
                    #[cfg(test)]
                    log::trace!("[RecvFuture::Poll] Cancelled 3");
                    return Poll::Ready(Result::Err(RxError::Cancelled));
                };
                // register the waker
                slot.as_mut()
                    .data_pinned()
                    .get_mut()
                    .replace(cx.waker().clone());
                let queue_pin = (*g).as_mut();
                let r = queue_pin.push_tail(slot.as_mut());
                debug_assert!(r.is_ok());
            }
            #[cfg(test)]
            log::trace!("[RecvFuture::Poll] Pending");
            return Poll::Pending;
        }

        if data_invalid && !tx_open {
            #[cfg(test)]
            log::trace!("[RecvFuture::Poll] Divorced");
            return Poll::Ready(Result::Err(RxError::Divorced(())));
        }
        if data_invalid {
            #[cfg(test)]
            log::trace!("[RecvFuture::Poll] Drained");
            return Poll::Ready(Result::Err(RxError::Drained(())));
        }

        // We need to move the data which might take some time. So acquiring
        // the mutex is a must.
        let mutex = oneshot.value_mutex();
        let acq = mutex.acquire();
        pin_mut!(acq);
        let Option::Some(mut g) = acq
            .lock()
            .may_cancel_with(this.cancel_.as_mut())
        else {
            #[cfg(test)]
            log::trace!("[RecvFuture::Poll] Cancelled 3");
            return Poll::Ready(Result::Err(RxError::Cancelled));
        };

        // try to read data with mutex acquired.
        let d = g.deref_mut();
        if let Result::Err(u) = oneshot.try_set_data_valid(false) {
            unreachable!("[RecvFuture::Poll] try_set_data_valid {u}")
        }
        let x = unsafe { d.as_ptr().read() };
        Poll::Ready(Result::Ok(x))
    }
}

type Uval = usize;
type FnTup = (fn(Uval) -> bool, fn(Uval) -> Uval);

pub(super) struct Flags<O: TrCmpxchOrderings>(
    AtomicFlags<Uval, <Uval as TrAtomicData>::AtomicCell, O>
);

impl<O: TrCmpxchOrderings> Flags<O> {
    const fn new(cell: <Uval as TrAtomicData>::AtomicCell) -> Self {
        Flags(AtomicFlags::new(cell))
    }

    /// Atomically load the flag value.
    #[inline(always)]
    fn value(&self) -> Uval {
        self.0.value()
    }

    fn can_send(&self) -> bool {
        let s = self.value();
        Self::expect_can_send(s)
    }

    fn can_receive(&self) -> bool {
        let s = self.value();
        Self::expect_can_receive(s)
    }

    fn can_peek(&self) -> bool {
        let s = self.value();
        Self::expect_can_peek(s)
    }

    fn is_data_ready(&self) -> bool {
        let s = self.value();
        Self::expect_data_valid(s)
    }

    fn has_tx_sent(&self) -> bool {
        let s = self.value();
        Self::expect_tx_expire(s)
    }

    fn is_tx_closed(&self) -> bool {
        let s = self.value();
        Self::expect_tx_closed(s)
    }

    /// Increase peeker count by fetch_add
    fn increase_peeker_count(&self) -> Uval {
        let r = self.0.try_spin_compare_exchange_weak(
            Self::expect_incr_peekers_count_legal,
            Self::desire_incr_peeker_count,
        );
        debug_assert!(r.is_succ());
        Self::read_peekers_count(r.into_inner())
    }

    fn decrease_peeker_count(&self) -> Uval {
        let r = self.0.try_spin_compare_exchange_weak(
            Self::expect_decr_peekers_count_legal,
            Self::desire_decr_peeker_count,
        );
        debug_assert!(r.is_succ());
        Self::read_peekers_count(r.into_inner())
    }

    /// Atomically load the peeker count.
    fn peeker_count(&self) -> Uval {
        Self::read_peekers_count(self.0.value())
    }

    fn try_set_data_valid(&self, is_valid: bool) -> Result<Uval, Uval> {
        let (expect, desire): FnTup = if is_valid {
            (Self::expect_data_invalid, Self::desire_data_valid)
        } else {
            (Self::expect_data_valid, Self::desire_data_invalid)
        };
        self.0.try_spin_compare_exchange_weak(expect, desire).into()
    }

    fn try_set_sent(&self) -> Result<Uval, Uval> {
        let desire = |s| {
            let s0 = Self::desire_data_valid(s);
            Self::desire_tx_expire(s0)
        };
        self.0
            .try_spin_compare_exchange_weak(Self::expect_can_send, desire)
            .into()
    }

    fn try_set_tx_closed(&self) -> Result<Uval, Uval> {
        let r = self.0.try_spin_compare_exchange_weak(
            Self::expect_tx_open,
            Self::desire_tx_closed,
        );
        r.into()
    }

    fn try_set_rx_closed(&self) -> Result<Uval, Uval> {
        let r = self.0.try_spin_compare_exchange_weak(
            Self::expect_rx_open,
            Self::desire_rx_closed,
        );
        r.into()
    }
}

impl<O: TrCmpxchOrderings> Flags<O> {
    const K_MUTEX_ACQ_FLAG: Uval = 1 << (Uval::BITS - 1);
    const K_DATA_VALID_FLAG: Uval = Self::K_MUTEX_ACQ_FLAG >> 1;
    const K_TX_EXPIRE_FLAG: Uval = Self::K_DATA_VALID_FLAG >> 1;
    const K_TX_CLOSED_FLAG: Uval = Self::K_TX_EXPIRE_FLAG >> 1;
    const K_RX_CLOSED_FLAG: Uval = Self::K_TX_CLOSED_FLAG >> 1;
    const K_PEEKERS_NUM_MASK: Uval = Self::K_RX_CLOSED_FLAG - 1;

    /// By default mutex_acq_flag is off
    fn expect_mutex_released(s: Uval) -> bool {
        !Self::expect_mutex_acquired(s)
    }
    fn expect_mutex_acquired(s: Uval) -> bool {
        s & Self::K_MUTEX_ACQ_FLAG == Self::K_MUTEX_ACQ_FLAG
    }
    fn desire_mutex_acquired(s: Uval) -> Uval {
        s | Self::K_MUTEX_ACQ_FLAG
    }
    fn desire_mutex_released(s: Uval) -> Uval {
        s & (!Self::K_MUTEX_ACQ_FLAG)
    }

    /// By default data valid flag is off
    fn expect_data_invalid(s: Uval) -> bool {
        !Self::expect_data_valid(s)
    }
    fn expect_data_valid(s: Uval) -> bool {
        s & Self::K_DATA_VALID_FLAG == Self::K_DATA_VALID_FLAG
    }
    fn desire_data_valid(s: Uval) -> Uval {
        s | Self::K_DATA_VALID_FLAG
    }
    fn desire_data_invalid(s: Uval) -> Uval {
        s & (!Self::K_DATA_VALID_FLAG)
    }

    /// By default tx expire flag is off
    fn expect_tx_expire(s: Uval) -> bool {
        s & Self::K_TX_EXPIRE_FLAG == Self::K_TX_EXPIRE_FLAG
    }
    fn expect_tx_not_expire(s: Uval) -> bool {
        !Self::expect_tx_expire(s)
    }
    fn desire_tx_expire(s: Uval) -> Uval {
        s | Self::K_TX_EXPIRE_FLAG
    }

    /// By default tx closed flag is off
    fn expect_tx_open(s: Uval) -> bool {
        !Self::expect_tx_closed(s)
    }
    fn expect_tx_closed(s: Uval) -> bool {
        s & Self::K_TX_CLOSED_FLAG == Self::K_TX_CLOSED_FLAG
    }
    fn desire_tx_closed(s: Uval) -> Uval {
        s | Self::K_TX_CLOSED_FLAG
    }

    /// By default rx closed flag is off
    fn expect_rx_open(s: Uval) -> bool {
        !Self::expect_rx_closed(s)
    }
    fn expect_rx_closed(s: Uval) -> bool {
        s & Self::K_RX_CLOSED_FLAG == Self::K_RX_CLOSED_FLAG
    }
    fn desire_rx_closed(s: Uval) -> Uval {
        s | Self::K_RX_CLOSED_FLAG
    }

    fn read_peekers_count(s: Uval) -> Uval {
        s & Self::K_PEEKERS_NUM_MASK
    }
    fn write_peekers_count(s: Uval, c: Uval) -> Uval {
        let c = c & Self::K_PEEKERS_NUM_MASK;
        let s = s &(!Self::K_PEEKERS_NUM_MASK);
        s | c
    }
    fn expect_incr_peekers_count_legal(s: Uval) -> bool {
        let c = Self::read_peekers_count(s);
        c < Self::K_PEEKERS_NUM_MASK
    }
    fn expect_decr_peekers_count_legal(s: Uval) -> bool {
        let c = Self::read_peekers_count(s);
        c > 0
    }
    fn desire_incr_peeker_count(s: Uval) -> Uval {
        let c = Self::read_peekers_count(s);
        Self::write_peekers_count(s, c + 1)
    }
    fn desire_decr_peeker_count(s: Uval) -> Uval {
        let c= Self::read_peekers_count(s);
        Self::write_peekers_count(s, c - 1)
    }

    fn expect_can_send(s: Uval) -> bool {
        Self::expect_data_invalid(s)
            && Self::expect_tx_not_expire(s)
            && Self::expect_rx_open(s)
    }

    fn expect_can_receive(s: Uval) -> bool {
        if !Self::expect_data_valid(s) {
            Self::expect_tx_open(s)
        } else {
            true
        }
    }

    fn expect_can_peek(s: Uval) -> bool {
        if !Self::expect_data_valid(s) {
            Self::expect_tx_open(s)
        } else {
            true
        }
    }
}

impl<O: TrCmpxchOrderings> TrMutexSignal<Uval> for Flags<O> {
    fn is_acquired(val: Uval) -> bool {
        Self::expect_mutex_acquired(val)
    }

    fn is_released(val: Uval) -> bool {
        Self::expect_mutex_released(val)
    }

    fn make_acquired(val: Uval) -> Uval {
        Self::desire_mutex_acquired(val)
    }

    fn make_released(val: Uval) -> Uval {
        Self::desire_mutex_released(val)
    }
}

impl<O: TrCmpxchOrderings> fmt::Debug for Flags<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.value();
        write!(f,
            "Acq({}), DataValid({}), TxClosed({}), RxClosed({}), PeekerNum({})",
            Self::expect_mutex_acquired(v),
            Self::expect_data_valid(v),
            Self::expect_tx_closed(v),
            Self::expect_rx_closed(v),
            Self::read_peekers_count(v),
        )
    }
}

#[cfg(test)]
mod tests_ {
    use std::{
        boxed::Box,
        future::IntoFuture,
        time::Duration,
        vec::Vec,
    };

    use abs_mm::mem_alloc::TrMalloc;
    use mm_ptr::{x_deps::abs_mm, Shared};

    use core_malloc::CoreAlloc;
    use super::*;

    fn flag_default_test_<O: TrCmpxchOrderings>(flags: &Flags<O>) {
        let v = flags.value();

        assert!(Flags::<O>::expect_mutex_released(v));

        // data is not valid by default
        assert!(Flags::<O>::expect_data_invalid(v));

        // tx expire flag is off by default
        assert!(Flags::<O>::expect_tx_not_expire(v));

        // tx is open by default
        assert!(Flags::<O>::expect_tx_open(v));

        // rx is open by default
        assert!(Flags::<O>::expect_rx_open(v));
    }

    #[test]
    fn flag_default_test() {
        let flags = Flags::<StrictOrderings>::new(AtomicUsize::new(0));
        flag_default_test_(&flags);
    }

    #[test]
    fn set_flag_test() {
        let flags = Flags::<StrictOrderings>::new(AtomicUsize::new(0));

        flag_default_test_(&flags);

        assert!(flags.try_set_data_valid(false).is_err());
        assert!(flags.try_set_data_valid(true).is_ok());
        assert!(flags.try_set_data_valid(true).is_err());
        assert!(flags.try_set_data_valid(false).is_ok());

        assert!(flags.try_set_tx_closed().is_ok());
        assert!(flags.try_set_tx_closed().is_err());
        assert!(flags.try_set_tx_closed().is_err());

        assert!(flags.try_set_rx_closed().is_ok());
        assert!(flags.try_set_rx_closed().is_err());
        assert!(flags.try_set_rx_closed().is_err());
    }

    #[tokio::test]
    async fn tx_rx_smoke() {
        let _ = env_logger::builder().is_test(true).try_init();

        let oneshot = Shared::new(
            Oneshot::<usize, StrictOrderings>::new(),
            CoreAlloc::new(),
        );

        self::flag_default_test_(&oneshot.flags_);
        assert!(!oneshot.is_triggered());
        assert!(!oneshot.is_data_ready());

        let Result::Ok((tx, rx)) = Oneshot
            ::try_split(oneshot, Shared::strong_count, Shared::weak_count)
        else {
            panic!("[oneshot::tests_::tx_rx_smoke] try_split failed");
        };
        pin_mut!(rx);
        assert!(tx.can_send());
        assert!(rx.can_receive());
        assert!(!rx.is_data_ready());

        const SEND_CONT: usize = 42;
        let recv_fut = rx.receive_async().into_future();
        assert!(tx.can_send());

        assert!(tx.send(SEND_CONT).wait().is_ok());
        let second_recv = recv_fut.await;
        assert!(second_recv.is_ok());
        assert!(second_recv.unwrap() == SEND_CONT);

        log::trace!("[tx_rx_smoke] {:?}", tx.oneshot().flags_);
        assert!(!tx.can_send());
        assert!(tx.send(0).wait().is_err());
    }

    #[tokio::test]
    async fn peeker_smoke() {
        let Result::Ok((tx, rx)) = Oneshot::try_split(
            Shared::new(
                Oneshot::<usize, StrictOrderings>::new(),
                CoreAlloc::new()),
            Shared::strong_count,
            Shared::weak_count,
        ) else {
            panic!("[oneshot::tests_::peeker_smoke] try_split failed");
        };
        assert!(tx.can_send());
        assert!(rx.can_receive());
        assert!(!rx.is_data_ready());

        const SEND_CONT: usize = 42;
        let Result::Ok(peeker) = rx.try_into() else { panic!() };
        pin_mut!(peeker);
        assert!(tx.can_send());
        assert!(peeker.can_peek());
        assert!(!peeker.is_data_ready());
        assert!(tx.send(SEND_CONT).wait().is_ok());
        let peek_res = peeker.as_mut().peek_async().await;
        assert!(peek_res.is_ok_and(|u| *u == SEND_CONT));

        assert!(!tx.can_send());
        assert!(tx.send(0).wait().is_err());
        assert!(peeker.can_peek());
        assert!(peeker.is_data_ready());
        assert!(peeker
            .try_peek()
            .ok()
            .flatten()
            .is_some_and(|u| *u == SEND_CONT)
        );
    }

    #[tokio::test]
    async fn direct_oneshot_peek_smoke() {
        async fn rx_work(channel: impl Deref<Target = Oneshot<(), StrictOrderings>>) {
            let peeker = channel.peeker();
            pin_mut!(peeker);
            log::trace!("[direct_oneshot_peek_smoke::rx_work]");
            let x = peeker.peek_async().await;
            assert!(x.is_ok())
        }
    
        async fn tx_work(channel: impl Deref<Target = Oneshot<(), StrictOrderings>>) {
            tokio::time::sleep(Duration::from_micros(300)).await;
            log::trace!("[direct_oneshot_peek_smoke::tx_work]");
            assert!(channel.send(()).wait().is_ok());
        }

        let chan = Shared::new(
            Oneshot::<(), StrictOrderings>::new(),
            CoreAlloc::new(),
        );
        let rx_hndl = tokio::task::spawn({
            let chan_cloned = chan.clone();
            rx_work(chan_cloned)
        });
        let tx_hndl = tokio::task::spawn({
            let chan_cloned = chan.clone();
            tx_work(chan_cloned)
        });
        assert!(rx_hndl.await.is_ok());
        assert!(tx_hndl.await.is_ok());
    }

    /// To test if multiple peekers each in an indivisual spawned task can work.
    #[tokio::test]
    async fn multiple_peeker_smoke() {
        async fn rx_work<P, O, A>(peeker: P)
        where
            P: Deref<Target = Peeker<Shared<Oneshot<(), O>, A>, (), O>>,
            O: TrCmpxchOrderings,
            A: TrMalloc + Clone,
        {
            let peeker = peeker.clone();
            pin_mut!(peeker);
            log::trace!("[multiple_peeker_smoke::rx_work]");
            let x = peeker.peek_async().await;
            assert!(x.is_ok())
        }

        async fn tx_work<S, O, A>(sender: S)
        where
            S: Deref<Target = Sender<Shared<Oneshot<(), O>, A>, (), O>>,
            O: TrCmpxchOrderings,
            A: TrMalloc + Clone,
        {
            tokio::time::sleep(Duration::from_micros(300)).await;
            log::trace!("[multiple_peeker_smoke::tx_work]");
            assert!(sender.send(()).wait().is_ok());
        }

        const PEEKER_CNT: usize = 16usize;

        let chan = Shared::new(
            Oneshot::<(), StrictOrderings>::new(),
            CoreAlloc::new(),
        );
        let (tx, rx) = Oneshot::
            try_split(
                chan,
                Shared::strong_count,
                Shared::weak_count)
            .unwrap();
        let tx = Box::new(tx);
        let rx = Shared::new(
            rx.try_into().unwrap(),
            CoreAlloc::new(),
        );
        let mut rx_handles = Vec::<tokio::task::JoinHandle<()>>::new();
        for _ in 0..PEEKER_CNT {
            let rx_hndl = tokio::task::spawn(rx_work(rx.clone()));
            rx_handles.push(rx_hndl);
        }
        let tx_hndl = tokio::task::spawn(tx_work(tx));
        assert!(tx_hndl.await.is_ok());
        for rx_hndl in rx_handles.into_iter() {
            assert!(rx_hndl.await.is_ok());
        }
    }
}
