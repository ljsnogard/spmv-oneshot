use core::{
    marker::PhantomData,
    pin::Pin,
    ops::{Deref, Try},
};

use abs_sync::{
    cancellation::TrCancellationToken,
    sync_tasks::TrSyncTask,
};
use atomex::TrCmpxchOrderings;
use pincol::x_deps::atomic_sync::x_deps::{abs_sync, atomex};

use super::oneshot_::*;

#[derive(Debug, PartialEq, Eq)]
pub enum TxError<T> {
    Cancelled,

    /// The receiver end is closed.
    Divorced(T),

    /// The sender has sent once and thus becomes disabled.
    Stuffed(T),
}

impl<T> TxError<T> {
    pub fn try_into_inner(self) -> Result<T, Self> {
        match self {
            TxError::Divorced(t) => Result::Ok(t),
            TxError::Stuffed(t) => Result::Ok(t),
            _ => Result::Err(self),
        }
    }

    pub fn map_inner<U>(self, op: impl FnOnce(T) -> U) -> TxError<U> {
        match self {
            TxError::Divorced(t) => TxError::Divorced(op(t)),
            TxError::Stuffed(t) => TxError::Stuffed(op(t)),
            _ => TxError::Cancelled,
        }
    }
}

#[derive(Debug)]
pub struct Sender<B, T, O>(B, PhantomData<Oneshot<T, O>>)
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings;

impl<B, T, O> Sender<B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    pub(super) fn new(oneshot: B) -> Self {
        Sender(oneshot, PhantomData)
    }

    #[inline]
    pub fn can_send(&self) -> bool {
        self.0.can_send()
    }

    #[inline]
    pub fn send(&self, data: T) -> SendTask<'_, T, O> {
        self.0.send(data)
    }

    pub(crate) fn oneshot(&self) -> &Oneshot<T, O> {
        self.0.deref()
    }
}

impl<B, T, O> Drop for Sender<B, T, O>
where
    B: Deref<Target = Oneshot<T, O>>,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        // Rx side may have exitted thus this is not guaranteed.
        let _ = self.0.try_set_tx_closed();
    }
}

pub struct SendTask<'a, T, O>
where
    O: TrCmpxchOrderings,
{
    oneshot_: &'a Oneshot<T, O>,
    data_: T,
}

impl<'a, T, O> SendTask<'a, T, O>
where
    O: TrCmpxchOrderings,
{
    pub(super) fn new(oneshot: &'a Oneshot<T, O>, data: T) -> Self {
        SendTask {
            oneshot_: oneshot,
            data_: data,
        }
    }

    pub fn may_cancel_with<C>(
        self,
        cancel: Pin<&mut C>,
    ) -> Result<(), TxError<T>>
    where
        C: TrCancellationToken,
    {
        self.oneshot_.send_with_cancel_(self.data_, cancel)
    }

    pub fn wait(self) -> Result<(), TxError<T>>{
        TrSyncTask::wait(self)
    }
}

impl<T, O> TrSyncTask for SendTask<'_, T, O>
where
    O: TrCmpxchOrderings,
{
    type Output = Result<(), TxError<T>>;

    fn may_cancel_with<C>(
        self,
        cancel: Pin<&mut C>,
    ) -> impl Try<Output = Self::Output>
    where
        C: TrCancellationToken,
    {
        Option::Some(SendTask::may_cancel_with(self, cancel))
    }
}
