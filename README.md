# spmv-oneshot

This crate provides an oneshot channels in `no_std` environment.

`Oneshot` channel is capable and only capable of a sending and receiving a single message.  

The `Receiver` of this channel can be converted to a `Peeker`, or more by cloning it.

```rust
use pin_utils::pin_mut;

use atomex::StrictOrderings;
use spmv_oneshot::{
    Oneshot,
    x_deps::{atomex, pin_utils},
};

let oneshot = Oneshot::<(), StrictOrderings>::new();
pin_mut!(oneshot);
let (tx, rx) = Oneshot::split(oneshot);
let peeker = rx.try_into().unwrap();
let _ = peeker.try_peek();
```