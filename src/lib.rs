// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.


//! # BackdropArc
//! 
//! An Arc (atomically reference counted smart pointer) that supports customized dropping strategies using [backdrop](https://crates.io/crates/backdrop).
//! 
//! `Arc<T, BackdropStrategy>` works very much like a `std::sync::Arc<T>`, except for two differences:
//! ### Drop strategies
//! 
//! When the last clone of a particular Arc goes out of scope, rather than dropping normally, the particular [BackdropStrategy](https://docs.rs/backdrop/latest/backdrop/trait.BackdropStrategy.html) is invoked. This way, dropping large or complex structures can be done in a background thread, background tokio task, delayed until later, etc.
//! 
//! This allows better reasoning about how long code using an Arc will take, since this is no longer dependent on 'do I own the last Arc or not?'.
//! 
//! ### No weak pointers => smaller arcs, predictable cleanup
//! 
//! [`std::sync::Arc<T>`] allows the usage of weak pointers. This is very helpful internally in self-referential structures (trees, graphs) but frequently not needed.
//! On the other hand, weak pointers are not 'free':
//! - They make every Arc instance bigger (3 words instead of 2), since instead of storing `(ptr, reference_count)` they need to store `(ptr, reference_count, weak_reference_count)`.
//! - They make dropping an `Arc<T>` more complex. The 'drop glue' of `T` will run once the last strong reference goes out of scope. But to not make Weak pointers dangle, the _deallocation_ of `T` only happens when the last `Weak` pointer goes out of scope. As you can imagine, this 'two part drop' interacts badly with `BackdropStrategy` where we want to e.g. move objects to a background thread on drop, because we need to make sure that the allocation of `T` lives long enough.
//! 
//! Therefore, `backdrop_arc` is modeled on the excellent [`triomphe`](https://crates.io/crates/triomphe) library.
//! Converting a [`Arc`] to and from a [`triomphe::Arc`] is a zero-cost operation, as the two types are guaranteed to have the same representation in memory.
//! (The same holds true for [`UniqueArc`] <-> [`triomphe::UniqueArc`])
//! 
//! Not supporting weak pointers enables a bunch of other features:
//! - [`Arc`] does not need any read-modify-update operations to handle the possibility of weak references.
//! - [`UniqueArc`] allows one to construct a temporarily-mutable Arc which can be converted to a regular [`Arc`] later.
//! - [`OffsetArc`] can be used transparently from C++ code and is compatible with (and can be converted to/from) [`Arc`].
//! - [`ArcBorrow`] is functionally similar to `&backdrop_arc::Arc<T>`, however in memory it's simply `&T`. This makes it more flexible for FFI; the source of the borrow need not be an Arc pinned on the stack (and can instead be a pointer from C++, or an `OffsetArc`). Additionally, this helps avoid pointer-chasing.
//! - [`Arc`] has can be constructed for dynamically-sized types via `from_header_and_iter`
//! - [`ArcUnion`] is union of two [`Arc`]s which fits inside one word of memory
//! # Features
//! 
//! - `backdrop_arc` supports no_std environments, as long as `alloc` is available, by disabling the (enabled by default) `std` feature.
//! - `serde`: Enables serialization/deserialization with the [`serde`](https://crates.io/crates/serde) crate.
//! - `stable_deref_trait`: Implements the `StableDeref` trait from the [`stable_deref_trait`](https://crates.io/crates/stable_deref_trait) crate for [`Arc`].
//! - `arc-swap`: Use [`Arc`] together with the [`arc-swap`](https://crates.io/crates/arc-swap) crate.
//! - `triomphe`: Convert (zero-cost) between [`triomphe::Arc`] <-> [`Arc`] (and [`UniqueArc`] <-> [`triomphe::UniqueArc`]).
//! - `unsize` use [`Arc`] together with the [`unsize`](https://crates.io/crates/unsize) crate.
//! 
//! 
//! ## Attribution
//! 
//! The source code of `backdrop_arc` is very heavily based on (and originally a fork of) `triomphe`, which itself originates from `servo_arc`.
//! 

#![allow(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate core;

#[cfg(feature = "arc-swap")]
extern crate arc_swap;
#[cfg(feature = "serde")]
extern crate serde;
#[cfg(feature = "stable_deref_trait")]
extern crate stable_deref_trait;
#[cfg(feature = "unsize")]
extern crate unsize;

extern crate backdrop;

/// Calculates the offset of the specified field from the start of the named struct.
/// This macro is impossible to be const until feature(const_ptr_offset_from) is stable.
macro_rules! offset_of {
    ($ty: path, $field: tt) => {{
        // ensure the type is a named struct
        // ensure the field exists and is accessible
        let $ty { $field: _, .. };

        let uninit = <::core::mem::MaybeUninit<$ty>>::uninit(); // const since 1.36

        let base_ptr: *const $ty = uninit.as_ptr(); // const since 1.59

        #[allow(unused_unsafe)]
        let field_ptr = unsafe { ::core::ptr::addr_of!((*base_ptr).$field) }; // since 1.51

        // // the const version requires feature(const_ptr_offset_from)
        // // https://github.com/rust-lang/rust/issues/92980
        // #[allow(unused_unsafe)]
        // unsafe { (field_ptr as *const u8).offset_from(base_ptr as *const u8) as usize }

        (field_ptr as usize) - (base_ptr as usize)
    }};
}

mod arc;
mod arc_borrow;
#[cfg(feature = "arc-swap")]
mod arc_swap_support;
mod arc_union;
mod header;
mod iterator_as_exact_size_iterator;
mod offset_arc;
// mod thin_arc;
mod unique_arc;

pub use arc::*;
pub use arc_borrow::*;
pub use arc_union::*;
pub use header::*;
pub use offset_arc::*;
// pub use thin_arc::*;
pub use backdrop::*;
pub use unique_arc::*;

#[cfg(feature = "std")]
use std::process::abort;

// `no_std`-compatible abort by forcing a panic while already panicing.
#[cfg(not(feature = "std"))]
#[cold]
fn abort() -> ! {
    struct PanicOnDrop;
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!()
        }
    }
    let _double_panicer = PanicOnDrop;
    panic!();
}
