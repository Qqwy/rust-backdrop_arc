use alloc::boxed::Box;
use core::fmt;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::Deref;
use core::ptr;

extern crate backdrop;
use crate::ArcInner;

use self::backdrop::BackdropStrategy;

use super::{Arc, ArcBorrow};

/// An `Arc`, except it holds a pointer to the T instead of to the
/// entire ArcInner.
///
/// An `OffsetArc<T, S>` has the same layout and ABI as a non-null
/// `const T*` in C, and may be used in FFI function signatures.
///
/// ```text
///  Arc<T, S>    OffsetArc<T, S>
///   |          |
///   v          v
///  ---------------------
/// | RefCount | T (data) | [ArcInner<T>]
///  ---------------------
/// ```
///
/// This means that this is a direct pointer to
/// its contained data (and can be read from by both C++ and Rust),
/// but we can also convert it to a "regular" `Arc<T, S>` by removing the offset.
///
/// This is very useful if you have an Arc-containing struct shared between Rust and C++,
/// and wish for C++ to be able to read the data behind the `Arc` without incurring
/// an FFI call overhead.
#[derive(Eq)]
#[repr(transparent)]
pub struct OffsetArc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    pub(crate) ptr: ptr::NonNull<T>,
    pub(crate) phantom: PhantomData<T>,
    pub(crate) phantom_strategy: PhantomData<S>,
}

unsafe impl<T: Sync + Send, S> Send for OffsetArc<T, S> where S: BackdropStrategy<Box<ArcInner<T>>> {}
unsafe impl<T: Sync + Send, S> Sync for OffsetArc<T, S> where S: BackdropStrategy<Box<ArcInner<T>>> {}

impl<T, S> Deref for OffsetArc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    type Target = T;
    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.ptr.as_ptr() }
    }
}

impl<T, S> Clone for OffsetArc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    #[inline]
    fn clone(&self) -> Self {
        Arc::into_raw_offset(self.clone_arc())
    }
}

impl<T, S> Drop for OffsetArc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    fn drop(&mut self) {
        let _ = Arc::<_, S>::from_raw_offset(OffsetArc {
            ptr: self.ptr,
            phantom: PhantomData,
            phantom_strategy: PhantomData,
        });
    }
}

impl<T: fmt::Debug, S> fmt::Debug for OffsetArc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: PartialEq, S> PartialEq for OffsetArc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    fn eq(&self, other: &OffsetArc<T, S>) -> bool {
        *(*self) == *(*other)
    }

    #[allow(clippy::partialeq_ne_impl)]
    fn ne(&self, other: &OffsetArc<T, S>) -> bool {
        *(*self) != *(*other)
    }
}

impl<T, S> OffsetArc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    /// Temporarily converts |self| into a bonafide Arc and exposes it to the
    /// provided callback. The refcount is not modified.
    #[inline]
    pub fn with_arc<F, U>(&self, f: F) -> U
    where
        F: FnOnce(&Arc<T, S>) -> U,
    {
        // Synthesize transient Arc, which never touches the refcount of the ArcInner.
        let transient = unsafe { ManuallyDrop::new(Arc::from_raw(self.ptr.as_ptr())) };

        // Expose the transient Arc to the callback, which may clone it if it wants
        // and forward the result to the user
        f(&transient)
    }

    /// If uniquely owned, provide a mutable reference
    /// Else create a copy, and mutate that
    ///
    /// This is functionally the same thing as `Arc::make_mut`
    #[inline]
    pub fn make_mut(&mut self) -> &mut T
    where
        T: Clone,
    {
        unsafe {
            // extract the OffsetArc as an owned variable
            let this = ptr::read(self);
            // treat it as a real Arc
            let mut arc = Arc::from_raw_offset(this);
            // obtain the mutable reference. Cast away the lifetime
            // This may mutate `arc`
            let ret = Arc::make_mut(&mut arc) as *mut _;
            // Store the possibly-mutated arc back inside, after converting
            // it to a OffsetArc again
            ptr::write(self, Arc::into_raw_offset(arc));
            &mut *ret
        }
    }

    /// Clone it as an `Arc`
    #[inline]
    pub fn clone_arc(&self) -> Arc<T, S> {
        OffsetArc::with_arc(self, |a| a.clone())
    }

    /// Produce a pointer to the data that can be converted back
    /// to an `Arc`
    #[inline]
    pub fn borrow_arc(&self) -> ArcBorrow<'_, T> {
        ArcBorrow(&**self)
    }
}
