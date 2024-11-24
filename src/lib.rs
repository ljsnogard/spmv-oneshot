#![no_std]

#![feature(type_alias_impl_trait)]
#![feature(try_trait_v2)]

#[cfg(test)]
extern crate std;

mod oneshot_;
mod peeker_;
mod recver_;
mod sender_;

pub use oneshot_::{Oneshot, RecvFuture};
pub use peeker_::{Peeker, PeekAsync, PeekFuture};
pub use recver_::{Receiver, RxError};
pub use sender_::{Sender, SendTask, TxError};

/// This exports the internal dependencies.
pub mod x_deps {
    pub use pincol;

    pub use pincol::x_deps::atomic_sync;
    pub use atomic_sync::x_deps::{abs_sync, atomex};
    pub use abs_sync::x_deps::pin_utils;
}
