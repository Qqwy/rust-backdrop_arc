use alloc::alloc::handle_alloc_error;
use alloc::boxed::Box;
use backdrop::Backdrop;
use core::alloc::Layout;
use core::borrow;
use core::cmp::Ordering;
use core::convert::From;
use core::ffi::c_void;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::iter::{FromIterator, FusedIterator};
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::ops::Deref;
use core::ptr::{self, NonNull};
use core::sync::atomic;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use core::{isize, usize};

extern crate backdrop;
use self::backdrop::BackdropStrategy;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "stable_deref_trait")]
use stable_deref_trait::{CloneStableDeref, StableDeref};

use crate::{abort, ArcBorrow, HeaderSlice, OffsetArc, UniqueArc};

/// A soft limit on the amount of references that may be made to an `Arc`.
///
/// Going above this limit will abort your program (although not
/// necessarily) at _exactly_ `MAX_REFCOUNT + 1` references.
const MAX_REFCOUNT: usize = (isize::MAX) as usize;

/// The internal object allocated by an Arc<T, S>.
///
/// (The structure which contains the reference count and `T` itself.)
///
/// Its internals are hidden, but the type is made public
/// because you will receive a `Box<ArcInner<T>>` when backdropping.
#[derive(Debug)]
#[repr(C)]
pub struct ArcInner<T: ?Sized> {
    pub(crate) count: atomic::AtomicUsize,
    pub(crate) data: T,
}

unsafe impl<T: ?Sized + Sync + Send> Send for ArcInner<T> {}
unsafe impl<T: ?Sized + Sync + Send> Sync for ArcInner<T> {}

/// An atomically reference counted shared pointer
///
/// See the documentation for [`Arc`] in the standard library. Unlike the
/// standard library `Arc`, this `Arc` does not support weak reference counting.
///
/// This `Arc` allows customizing how it is dropped.
/// An `backdrop_arc::Arc<T, S>` behaves much like a [`Arc<backdrop::Backdrop<Box<T>, S>>`],
/// in that the backdrop strategy is executed _when the last Arc clone goes out of scope_.
/// The difference with [`Arc<backdrop::Backdrop<Box<T>, S>>`] is that there is no double pointer-indirection (arc -> box -> T), managing the allocated `T` is done directly in the Arc.
///
/// Basic usage is as follows:
/// ```
/// # #[cfg(feature = "std")] {
/// use backdrop_arc::Arc;
/// use backdrop_arc::{DebugStrategy, TrivialStrategy};
///
/// // Either specify the return type:
/// let mynum: Arc<usize, DebugStrategy<TrivialStrategy>> = Arc::new(42);
///
/// // Or use the 'Turbofish' syntax on the function call:
/// let mynum2 = Arc::<_, DebugStrategy<TrivialStrategy>>::new(42);
///
/// assert_eq!(mynum, mynum2);
/// // <- Because we are using the DebugStrategy, info is printed when the arcs go out of scope
/// # }
/// ```
///
/// See [`backdrop::Backdrop`] for more info.
///
/// [`Arc`]: https://doc.rust-lang.org/stable/std/sync/struct.Arc.html
#[repr(transparent)]
pub struct Arc<T: ?Sized, S: BackdropStrategy<Box<ArcInner<T>>>> {
    pub(crate) p: ptr::NonNull<ArcInner<T>>,
    pub(crate) phantom: PhantomData<T>,
    pub(crate) phantom_strategy: PhantomData<S>,
}

unsafe impl<T: ?Sized + Sync + Send, S> Send for Arc<T, S> where
    S: BackdropStrategy<Box<ArcInner<T>>>
{
}
unsafe impl<T: ?Sized + Sync + Send, S> Sync for Arc<T, S> where
    S: BackdropStrategy<Box<ArcInner<T>>>
{
}

impl<T, S: BackdropStrategy<Box<ArcInner<T>>>> Arc<T, S> {
    /// Construct an `Arc<T, S>`
    #[inline]
    pub fn new(data: T) -> Self {
        let ptr = Box::into_raw(Box::new(ArcInner {
            count: atomic::AtomicUsize::new(1),
            data,
        }));

        unsafe {
            Arc {
                p: ptr::NonNull::new_unchecked(ptr),
                phantom: PhantomData,
                phantom_strategy: PhantomData,
            }
        }
    }

    /// Alter the strategy that is used for an Arc<T, S> to another.
    /// This is a zero-cost operation.
    pub fn with_strategy<S2: BackdropStrategy<Box<ArcInner<T>>>>(arc: Arc<T, S>) -> Arc<T, S2> {
        // Safety: S and S2 are ZSTs which only do something at drop-time
        unsafe { core::mem::transmute(arc) }
    }

    /// Reconstruct the Arc<T, S> from a raw pointer obtained from into_raw()
    ///
    /// Note: This raw pointer will be offset in the allocation and must be preceded
    /// by the atomic count.
    ///
    /// It is recommended to use OffsetArc for this
    #[inline]
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        // FIXME: when `byte_sub` is stabilized, this can accept T: ?Sized.

        // To find the corresponding pointer to the `ArcInner` we need
        // to subtract the offset of the `data` field from the pointer.
        let ptr = (ptr as *const u8).sub(offset_of!(ArcInner<T>, data));
        Arc::from_raw_inner(ptr as *mut ArcInner<T>)
    }

    /// Temporarily converts |self| into a bonafide OffsetArc and exposes it to the
    /// provided callback. The refcount is not modified.
    #[inline(always)]
    pub fn with_raw_offset_arc<F, U>(&self, f: F) -> U
    where
        F: FnOnce(&OffsetArc<T, S>) -> U,
    {
        // Synthesize transient Arc, which never touches the refcount of the ArcInner.
        // Store transient in `ManuallyDrop`, to leave the refcount untouched.
        let transient = unsafe { ManuallyDrop::new(Arc::into_raw_offset(ptr::read(self))) };

        // Expose the transient Arc to the callback, which may clone it if it wants.
        f(&transient)
    }

    /// Converts an `Arc` into a `OffsetArc`. This consumes the `Arc`, so the refcount
    /// is not modified.
    #[inline]
    pub fn into_raw_offset(a: Self) -> OffsetArc<T, S> {
        unsafe {
            OffsetArc {
                ptr: ptr::NonNull::new_unchecked(Arc::into_raw(a) as *mut T),
                phantom: PhantomData,
                phantom_strategy: PhantomData,
            }
        }
    }

    /// Converts a `OffsetArc` into an `Arc`. This consumes the `OffsetArc`, so the refcount
    /// is not modified.
    #[inline]
    pub fn from_raw_offset(a: OffsetArc<T, S>) -> Self {
        let a = ManuallyDrop::new(a);
        let ptr = a.ptr.as_ptr();
        unsafe { Arc::from_raw(ptr) }
    }

    /// Returns the inner value, if the [`Arc`] has exactly one strong reference.
    ///
    /// Otherwise, an [`Err`] is returned with the same [`Arc`] that was
    /// passed in.
    ///
    /// # Examples
    ///
    /// ```
    /// use backdrop_arc::{Arc, TrivialStrategy};
    ///
    /// let x: Arc<usize, TrivialStrategy> = Arc::new(3);
    /// assert_eq!(Arc::try_unwrap(x), Ok(3));
    ///
    /// let x: Arc<usize, TrivialStrategy> = Arc::new(4);
    /// let _y = Arc::clone(&x);
    /// assert_eq!(*Arc::try_unwrap(x).unwrap_err(), 4);
    /// ```
    pub fn try_unwrap(this: Self) -> Result<T, Self> {
        Self::try_unique(this).map(UniqueArc::into_inner)
    }
}

impl<T, S: BackdropStrategy<Box<ArcInner<[T]>>>> Arc<[T], S> {
    /// Reconstruct the `Arc<[T]>` from a raw pointer obtained from `into_raw()`.
    ///
    /// [`Arc::from_raw`] should accept unsized types, but this is not trivial to do correctly
    /// until the feature [`pointer_bytes_offsets`](https://github.com/rust-lang/rust/issues/96283)
    /// is stabilized. This is stopgap solution for slices.
    pub unsafe fn from_raw_slice(ptr: *const [T]) -> Self {
        let len = (*ptr).len();
        // Assuming the offset of `T` in `ArcInner<T>` is the same
        // as as offset of `[T]` in `ArcInner<[T]>`.
        // (`offset_of!` macro requires `Sized`.)
        let arc_inner_ptr = (ptr as *const u8).sub(offset_of!(ArcInner<T>, data));
        // Synthesize the fat pointer: the pointer metadata for `Arc<[T]>`
        // is the same as the pointer metadata for `[T]`: the length.
        let fake_slice = ptr::slice_from_raw_parts_mut(arc_inner_ptr as *mut T, len);
        Arc::from_raw_inner(fake_slice as *mut ArcInner<[T]>)
    }
}

impl<T: ?Sized, S: BackdropStrategy<Box<ArcInner<T>>>> Arc<T, S> {
    /// Convert the Arc<T, S> to a raw pointer, suitable for use across FFI
    ///
    /// Note: This returns a pointer to the data T, which is offset in the allocation.
    ///
    /// It is recommended to use OffsetArc for this.
    #[inline]
    pub fn into_raw(this: Self) -> *const T {
        let this = ManuallyDrop::new(this);
        this.as_ptr()
    }

    /// Returns the raw pointer.
    ///
    /// Same as into_raw except `self` isn't consumed.
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        // SAFETY: This cannot go through a reference to `data`, because this method
        // is used to implement `into_raw`. To reconstruct the full `Arc` from this
        // pointer, it needs to maintain its full provenance, and not be reduced to
        // just the contained `T`.
        unsafe { ptr::addr_of_mut!((*self.ptr()).data) }
    }

    /// Produce a pointer to the data that can be converted back
    /// to an Arc. This is basically an `&Arc<T, S>`, without the extra indirection.
    /// It has the benefits of an `&T` but also knows about the underlying refcount
    /// and can be converted into more `Arc<T, S>`s if necessary.
    #[inline]
    pub fn borrow_arc(&self) -> ArcBorrow<'_, T> {
        ArcBorrow(&**self)
    }

    /// Returns the address on the heap of the Arc itself -- not the T within it -- for memory
    /// reporting.
    pub fn heap_ptr(&self) -> *const c_void {
        self.p.as_ptr() as *const ArcInner<T> as *const c_void
    }

    #[inline]
    pub(super) fn into_raw_inner(this: Self) -> *mut ArcInner<T> {
        let this = ManuallyDrop::new(this);
        this.ptr()
    }

    /// Construct an `Arc` from an allocated `ArcInner`.
    /// # Safety
    /// The `ptr` must point to a valid instance, allocated by an `Arc`. The reference could will
    /// not be modified.
    pub(super) unsafe fn from_raw_inner(ptr: *mut ArcInner<T>) -> Self {
        Arc {
            p: ptr::NonNull::new_unchecked(ptr),
            phantom: PhantomData,
            phantom_strategy: PhantomData,
        }
    }

    #[inline]
    pub(super) fn inner(&self) -> &ArcInner<T> {
        // This unsafety is ok because while this arc is alive we're guaranteed
        // that the inner pointer is valid. Furthermore, we know that the
        // `ArcInner` structure itself is `Sync` because the inner data is
        // `Sync` as well, so we're ok loaning out an immutable pointer to these
        // contents.
        unsafe { &*self.ptr() }
    }

    // Non-inlined part of `drop`. Just invokes the destructor.
    #[inline(never)]
    unsafe fn drop_slow(&mut self) {
        let _ = Backdrop::<_, S>::new(Box::from_raw(self.ptr()));
    }

    /// Test pointer equality between the two Arcs, i.e. they must be the _same_
    /// allocation
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.ptr() == other.ptr()
    }

    pub(crate) fn ptr(&self) -> *mut ArcInner<T> {
        self.p.as_ptr()
    }

    /// Allocates an `ArcInner<T>` with sufficient space for
    /// a possibly-unsized inner value where the value has the layout provided.
    ///
    /// The function `mem_to_arcinner` is called with the data pointer
    /// and must return back a (potentially fat)-pointer for the `ArcInner<T>`.
    ///
    /// ## Safety
    ///
    /// `mem_to_arcinner` must return the same pointer, the only things that can change are
    /// - its type
    /// - its metadata
    ///
    /// `value_layout` must be correct for `T`.
    #[allow(unused_unsafe)]
    pub(super) unsafe fn allocate_for_layout(
        value_layout: Layout,
        mem_to_arcinner: impl FnOnce(*mut u8) -> *mut ArcInner<T>,
    ) -> NonNull<ArcInner<T>> {
        let layout = Layout::new::<ArcInner<()>>()
            .extend(value_layout)
            .unwrap()
            .0
            .pad_to_align();

        // Safety: we propagate safety requirements to the caller
        unsafe {
            Arc::<_, S>::try_allocate_for_layout(value_layout, mem_to_arcinner)
                .unwrap_or_else(|_| handle_alloc_error(layout))
        }
    }

    /// Allocates an `ArcInner<T>` with sufficient space for
    /// a possibly-unsized inner value where the value has the layout provided,
    /// returning an error if allocation fails.
    ///
    /// The function `mem_to_arcinner` is called with the data pointer
    /// and must return back a (potentially fat)-pointer for the `ArcInner<T>`.
    ///
    /// ## Safety
    ///
    /// `mem_to_arcinner` must return the same pointer, the only things that can change are
    /// - its type
    /// - its metadata
    ///
    /// `value_layout` must be correct for `T`.
    #[allow(unused_unsafe)]
    unsafe fn try_allocate_for_layout(
        value_layout: Layout,
        mem_to_arcinner: impl FnOnce(*mut u8) -> *mut ArcInner<T>,
    ) -> Result<NonNull<ArcInner<T>>, ()> {
        let layout = Layout::new::<ArcInner<()>>()
            .extend(value_layout)
            .unwrap()
            .0
            .pad_to_align();

        let ptr = NonNull::new(alloc::alloc::alloc(layout)).ok_or(())?;

        // Initialize the ArcInner
        let inner = mem_to_arcinner(ptr.as_ptr());
        debug_assert_eq!(unsafe { Layout::for_value(&*inner) }, layout);

        unsafe {
            ptr::write(&mut (*inner).count, atomic::AtomicUsize::new(1));
        }

        // Safety: `ptr` is checked to be non-null,
        //         `inner` is the same as `ptr` (per the safety requirements of this function)
        unsafe { Ok(NonNull::new_unchecked(inner)) }
    }
}

impl<H, T, S: BackdropStrategy<Box<ArcInner<HeaderSlice<H, [T]>>>>> Arc<HeaderSlice<H, [T]>, S> {
    pub(super) fn allocate_for_header_and_slice(
        len: usize,
    ) -> NonNull<ArcInner<HeaderSlice<H, [T]>>> {
        let layout = Layout::new::<H>()
            .extend(Layout::array::<T>(len).unwrap())
            .unwrap()
            .0
            .pad_to_align();

        unsafe {
            // Safety:
            // - the provided closure does not change the pointer (except for meta & type)
            // - the provided layout is valid for `HeaderSlice<H, [T]>`
            Arc::<_, S>::allocate_for_layout(layout, |mem| {
                // Synthesize the fat pointer. We do this by claiming we have a direct
                // pointer to a [T], and then changing the type of the borrow. The key
                // point here is that the length portion of the fat pointer applies
                // only to the number of elements in the dynamically-sized portion of
                // the type, so the value will be the same whether it points to a [T]
                // or something else with a [T] as its last member.
                let fake_slice = ptr::slice_from_raw_parts_mut(mem as *mut T, len);
                fake_slice as *mut ArcInner<HeaderSlice<H, [T]>>
            })
        }
    }
}

impl<T, S> Arc<MaybeUninit<T>, S>
where
    S: BackdropStrategy<Box<ArcInner<MaybeUninit<T>>>>,
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    /// Create an Arc contains an `MaybeUninit<T>`.
    pub fn new_uninit() -> Self {
        Arc::new(MaybeUninit::<T>::uninit())
    }

    /// Calls `MaybeUninit::write` on the value contained.
    ///
    /// ## Panics
    ///
    /// If the `Arc` is not unique.
    #[deprecated(
        since = "0.1.7",
        note = "this function previously was UB and now panics for non-unique `Arc`s. Use `UniqueArc::write` instead."
    )]
    #[track_caller]
    pub fn write(&mut self, val: T) -> &mut T {
        UniqueArc::write(must_be_unique(self), val)
    }

    /// Obtain a mutable pointer to the stored `MaybeUninit<T>`.
    pub fn as_mut_ptr(&mut self) -> *mut MaybeUninit<T> {
        unsafe { &mut (*self.ptr()).data }
    }

    /// # Safety
    ///
    /// Must initialize all fields before calling this function.
    #[inline]
    pub unsafe fn assume_init(self) -> Arc<T, S> {
        Arc::from_raw_inner(ManuallyDrop::new(self).ptr().cast())
    }
}

impl<T, S> Arc<[MaybeUninit<T>], S>
where
    S: BackdropStrategy<Box<ArcInner<HeaderSlice<(), [MaybeUninit<T>]>>>>,
    S: BackdropStrategy<Box<ArcInner<[MaybeUninit<T>]>>>,
    S: BackdropStrategy<Box<ArcInner<[T]>>>,
{
    /// Create an Arc contains an array `[MaybeUninit<T>]` of `len`.
    pub fn new_uninit_slice(len: usize) -> Self {
        UniqueArc::new_uninit_slice(len).shareable()
    }

    /// Obtain a mutable slice to the stored `[MaybeUninit<T>]`.
    #[deprecated(
        since = "0.1.8",
        note = "this function previously was UB and now panics for non-unique `Arc`s. Use `UniqueArc` or `get_mut` instead."
    )]
    #[track_caller]
    pub fn as_mut_slice(&mut self) -> &mut [MaybeUninit<T>] {
        must_be_unique(self)
    }

    /// # Safety
    ///
    /// Must initialize all fields before calling this function.
    #[inline]
    pub unsafe fn assume_init(self) -> Arc<[T], S> {
        Arc::from_raw_inner(ManuallyDrop::new(self).ptr() as _)
    }
}

impl<T: ?Sized, S> Clone for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    #[inline]
    fn clone(&self) -> Self {
        // Using a relaxed ordering is alright here, as knowledge of the
        // original reference prevents other threads from erroneously deleting
        // the object.
        //
        // As explained in the [Boost documentation][1], Increasing the
        // reference counter can always be done with memory_order_relaxed: New
        // references to an object can only be formed from an existing
        // reference, and passing an existing reference from one thread to
        // another must already provide any required synchronization.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        let old_size = self.inner().count.fetch_add(1, Relaxed);

        // However we need to guard against massive refcounts in case someone
        // is `mem::forget`ing Arcs. If we don't do this the count can overflow
        // and users will use-after free. We racily saturate to `isize::MAX` on
        // the assumption that there aren't ~2 billion threads incrementing
        // the reference count at once. This branch will never be taken in
        // any realistic program.
        //
        // We abort because such a program is incredibly degenerate, and we
        // don't care to support it.
        if old_size > MAX_REFCOUNT {
            abort();
        }

        unsafe {
            Arc {
                p: ptr::NonNull::new_unchecked(self.ptr()),
                phantom: PhantomData,
                phantom_strategy: PhantomData,
            }
        }
    }
}

/// Iterator type to give out many clones an arc without the overhead of calling `clone` every time.
///
/// Return type of [`Arc::clone_many`].
///
/// This iterator will increase the refcount by the desired amount _in one atomic operation_ during creation,
/// and if the iterator is dropped before that many arcs are extracted from it,
/// we decrease the refcount by the leftover amount _in one atomic operation_ to make sure the arc is not leaked.
///
/// (if the iterator is empty, this step is of course skipped)
#[derive(Debug, Hash, Clone)]
pub struct ArcCloneIter<'a, T: ?Sized, S: BackdropStrategy<Box<ArcInner<T>>>> {
    orig: &'a Arc<T, S>,
    arcs_left: usize,
}

impl<'a, T: ?Sized, S: BackdropStrategy<Box<ArcInner<T>>>> ArcCloneIter<'a, T, S> {
    #[inline]
    fn new(orig: &'a Arc<T, S>, count: usize) -> Self {
        // Just like inside `clone`, we can use a Relaxed ordering:
        // Being passed `orig: &Arc<T, S>` ensures that for the duration of this function
        // (and the lifetime of the ArcCloneIter it returns),
        // `orig` has a refcount higher than 0.
        // Therefore other threads will not erroneously delete it before the critical section is over
        let _ = orig.inner().count.fetch_update(Relaxed, Relaxed, |c| {
            // Two safety checks are necessary:
            // 1) abort if we overflow the full usize::MAX space
            // necessary since we increase by a large step
            let val = c.checked_add(count).unwrap_or_else(|| abort());
            // 2) abort if we overflow the MAX_REFCOUNT, just like normal `clone()`
            if val > MAX_REFCOUNT {
                abort();
            }
            Some(val)
        });

        Self {
            orig,
            arcs_left: count,
        }
    }
}

impl<'a, T: ?Sized, S: BackdropStrategy<Box<ArcInner<T>>>> Drop for ArcCloneIter<'a, T, S> {
    #[inline]
    fn drop(&mut self) {
        // If no arcs are left, no cleanup is necessary
        if self.arcs_left == 0 {
            return;
        }

        // Otherwise, make sure we decrease the refcount by the leftover amount
        // Note that we don't need to check whether we reach refcount 0 (and then drop the contents of the arc):
        // since we have the reference `orig`, the refcount will always be > 0
        let _ = self.orig.inner().count.fetch_update(Relaxed, Relaxed, |c| {
            // Abort if we underflow
            let val = c.checked_sub(self.arcs_left).unwrap_or_else(|| abort());
            Some(val)
        });
    }
}

impl<'a, T: ?Sized, S: BackdropStrategy<Box<ArcInner<T>>>> Iterator for ArcCloneIter<'a, T, S> {
    type Item = Arc<T, S>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.arcs_left == 0 {
            return None;
        }
        self.arcs_left -= 1;

        // SAFETY: we only make a new arc when there still are refcounts left to give out
        let new_arc = unsafe {
            Arc {
                p: ptr::NonNull::new_unchecked(self.orig.ptr()),
                phantom: PhantomData,
                phantom_strategy: PhantomData,
            }
        };
        Some(new_arc)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.arcs_left, Some(self.arcs_left))
    }
}

impl<'a, T: ?Sized, S: BackdropStrategy<Box<ArcInner<T>>>> FusedIterator
    for ArcCloneIter<'a, T, S>
{
}

impl<T: ?Sized, S> Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    /// Optimization over calling `clone()` many times:
    ///
    /// Instead of incrementing the reference count by one many times
    /// (which requires an atomic barrier each time)
    /// we increase the reference count by `inc` _once_,
    /// needing only a single atomic barrier.
    ///
    ///
    /// # Failure scenarios
    /// - Aborts if increasing the reference count by `inc` results in a refcount higher than isize::MAX,
    ///   to make sure the refcount never overflows.
    ///   (The only way to trigger this in a program is by `mem::forget`ting Arcs in a loop).
    ///
    ///
    /// # Examples
    ///
    /// The resulting iterator gives out exactly `count` `Arc`s:
    /// ```
    /// use backdrop_arc::{Arc, TrivialStrategy};
    ///
    /// let myarc: Arc<u32, TrivialStrategy> = Arc::new(42);
    /// let many_clones: Vec<_> = Arc::clone_many(&myarc, 1000).collect();
    /// assert_eq!(Arc::count(&myarc), 1001);
    /// ```
    ///
    /// If the iterator is dropped before all of them are given out,
    /// the reference count is decreased by the leftover amount (also in one atomic barier):
    ///
    /// ```
    /// use backdrop_arc::{Arc, TrivialStrategy};
    ///
    /// let myarc: Arc<u32, TrivialStrategy> = Arc::new(42);
    /// let many_clones: Vec<_> = Arc::clone_many(&myarc, 1000).take(100).collect();
    /// assert_eq!(Arc::count(&myarc), 101);
    /// ```
    pub fn clone_many<'a>(this: &'a Self, count: usize) -> ArcCloneIter<'a, T, S> {
        ArcCloneIter::new(this, count)
    }
}

impl<T: ?Sized, S> Deref for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.inner().data
    }
}

impl<T: Clone, S> Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    /// Makes a mutable reference to the `Arc`, cloning if necessary
    ///
    /// This is functionally equivalent to [`Arc::make_mut`][mm] from the standard library.
    ///
    /// If this `Arc` is uniquely owned, `make_mut()` will provide a mutable
    /// reference to the contents. If not, `make_mut()` will create a _new_ `Arc`
    /// with a copy of the contents, update `this` to point to it, and provide
    /// a mutable reference to its contents.
    ///
    /// This is useful for implementing copy-on-write schemes where you wish to
    /// avoid copying things if your `Arc` is not shared.
    ///
    /// [mm]: https://doc.rust-lang.org/stable/std/sync/struct.Arc.html#method.make_mut
    #[inline]
    pub fn make_mut(this: &mut Self) -> &mut T {
        if !this.is_unique() {
            // Another pointer exists; clone
            *this = Arc::new(T::clone(&this));
        }

        unsafe {
            // This unsafety is ok because we're guaranteed that the pointer
            // returned is the *only* pointer that will ever be returned to T. Our
            // reference count is guaranteed to be 1 at this point, and we required
            // the Arc itself to be `mut`, so we're returning the only possible
            // reference to the inner data.
            &mut (*this.ptr()).data
        }
    }

    /// Makes a `UniqueArc` from an `Arc`, cloning if necessary.
    ///
    /// If this `Arc` is uniquely owned, `make_unique()` will provide a `UniqueArc`
    /// containing `this`. If not, `make_unique()` will create a _new_ `Arc`
    /// with a copy of the contents, update `this` to point to it, and provide
    /// a `UniqueArc` to it.
    ///
    /// This is useful for implementing copy-on-write schemes where you wish to
    /// avoid copying things if your `Arc` is not shared.
    #[inline]
    pub fn make_unique(this: &mut Self) -> &mut UniqueArc<T, S> {
        if !this.is_unique() {
            // Another pointer exists; clone
            *this = Arc::new(T::clone(&this));
        }

        unsafe {
            // Safety: this is either unique or just created (which is also unique)
            UniqueArc::from_arc_ref(this)
        }
    }

    /// If we have the only reference to `T` then unwrap it. Otherwise, clone `T` and return the clone.
    ///
    /// Assuming `arc_t` is of type `Arc<T, S>`, this function is functionally equivalent to `(*arc_t).clone()`, but will avoid cloning the inner value where possible.
    pub fn unwrap_or_clone(this: Arc<T, S>) -> T {
        Self::try_unwrap(this).unwrap_or_else(|this| T::clone(&this))
    }
}

impl<T: ?Sized, S> Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    /// Provides mutable access to the contents _if_ the `Arc` is uniquely owned.
    #[inline]
    pub fn get_mut(this: &mut Self) -> Option<&mut T> {
        if this.is_unique() {
            unsafe {
                // See make_mut() for documentation of the threadsafety here.
                Some(&mut (*this.ptr()).data)
            }
        } else {
            None
        }
    }

    /// Provides unique access to the arc _if_ the `Arc` is uniquely owned.
    pub fn get_unique(this: &mut Self) -> Option<&mut UniqueArc<T, S>> {
        Self::try_as_unique(this).ok()
    }

    /// Whether or not the `Arc` is uniquely owned (is the refcount 1?).
    pub fn is_unique(&self) -> bool {
        // See the extensive discussion in [1] for why this needs to be Acquire.
        //
        // [1] https://github.com/servo/servo/issues/21186
        Self::count(self) == 1
    }

    /// Gets the number of [`Arc`] pointers to this allocation
    pub fn count(this: &Self) -> usize {
        this.inner().count.load(Acquire)
    }

    /// Returns a [`UniqueArc`] if the [`Arc`] has exactly one strong reference.
    ///
    /// Otherwise, an [`Err`] is returned with the same [`Arc`] that was
    /// passed in.
    ///
    /// # Examples
    ///
    /// ```
    /// use backdrop_arc::{Arc, UniqueArc, TrivialStrategy};
    ///
    /// let x: Arc<usize, TrivialStrategy> = Arc::new(3);
    /// assert_eq!(UniqueArc::into_inner(Arc::try_unique(x).unwrap()), 3);
    ///
    /// let x: Arc<usize, TrivialStrategy> = Arc::new(4);
    /// let _y = Arc::clone(&x);
    /// assert_eq!(
    ///     *Arc::try_unique(x).map(UniqueArc::into_inner).unwrap_err(),
    ///     4,
    /// );
    /// ```
    pub fn try_unique(this: Self) -> Result<UniqueArc<T, S>, Self> {
        if this.is_unique() {
            // Safety: The current arc is unique and making a `UniqueArc`
            //         from it is sound
            unsafe { Ok(UniqueArc::from_arc(this)) }
        } else {
            Err(this)
        }
    }

    pub(crate) fn try_as_unique(this: &mut Self) -> Result<&mut UniqueArc<T, S>, &mut Self> {
        if this.is_unique() {
            // Safety: The current arc is unique and making a `UniqueArc`
            //         from it is sound
            unsafe { Ok(UniqueArc::from_arc_ref(this)) }
        } else {
            Err(this)
        }
    }
}

impl<T: ?Sized, S> Drop for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    #[inline]
    fn drop(&mut self) {
        // Because `fetch_sub` is already atomic, we do not need to synchronize
        // with other threads unless we are going to delete the object.
        if self.inner().count.fetch_sub(1, Release) != 1 {
            return;
        }

        // FIXME(bholley): Use the updated comment when [2] is merged.
        //
        // This load is needed to prevent reordering of use of the data and
        // deletion of the data.  Because it is marked `Release`, the decreasing
        // of the reference count synchronizes with this `Acquire` load. This
        // means that use of the data happens before decreasing the reference
        // count, which happens before this load, which happens before the
        // deletion of the data.
        //
        // As explained in the [Boost documentation][1],
        //
        // > It is important to enforce any possible access to the object in one
        // > thread (through an existing reference) to *happen before* deleting
        // > the object in a different thread. This is achieved by a "release"
        // > operation after dropping a reference (any access to the object
        // > through this reference must obviously happened before), and an
        // > "acquire" operation before deleting the object.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        // [2]: https://github.com/rust-lang/rust/pull/41714
        self.inner().count.load(Acquire);

        unsafe {
            self.drop_slow();
        }
    }
}

impl<T: ?Sized + PartialEq, S> PartialEq for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    fn eq(&self, other: &Arc<T, S>) -> bool {
        Self::ptr_eq(self, other) || *(*self) == *(*other)
    }

    #[allow(clippy::partialeq_ne_impl)]
    fn ne(&self, other: &Arc<T, S>) -> bool {
        !Self::ptr_eq(self, other) && *(*self) != *(*other)
    }
}

impl<T: ?Sized + PartialOrd, S> PartialOrd for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    fn partial_cmp(&self, other: &Arc<T, S>) -> Option<Ordering> {
        (**self).partial_cmp(&**other)
    }

    fn lt(&self, other: &Arc<T, S>) -> bool {
        *(*self) < *(*other)
    }

    fn le(&self, other: &Arc<T, S>) -> bool {
        *(*self) <= *(*other)
    }

    fn gt(&self, other: &Arc<T, S>) -> bool {
        *(*self) > *(*other)
    }

    fn ge(&self, other: &Arc<T, S>) -> bool {
        *(*self) >= *(*other)
    }
}

impl<T: ?Sized + Ord, S> Ord for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    fn cmp(&self, other: &Arc<T, S>) -> Ordering {
        (**self).cmp(&**other)
    }
}

impl<T: ?Sized + Eq, S> Eq for Arc<T, S> where S: BackdropStrategy<Box<ArcInner<T>>> {}

impl<T: ?Sized + fmt::Display, S> fmt::Display for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Debug, S> fmt::Debug for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized, S> fmt::Pointer for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Pointer::fmt(&self.ptr(), f)
    }
}

impl<T: Default, S> Default for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    #[inline]
    fn default() -> Arc<T, S> {
        Arc::new(Default::default())
    }
}

impl<T: ?Sized + Hash, S> Hash for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl<T, S: BackdropStrategy<Box<ArcInner<T>>>> From<T> for Arc<T, S> {
    #[inline]
    fn from(t: T) -> Self {
        Arc::new(t)
    }
}

#[cfg(feature = "triomphe")]
extern crate triomphe;

#[cfg(feature = "triomphe")]
/// Converting to- and from a [`triomphe::Arc<T>`] is a zero-cost operation
impl<T, S: BackdropStrategy<Box<ArcInner<T>>>> From<triomphe::Arc<T>> for Arc<T, S> {
    #[inline]
    fn from(arc: triomphe::Arc<T>) -> Self {
        unsafe { core::mem::transmute(arc) }
    }
}

#[cfg(feature = "triomphe")]
/// Converting to- and from a [`triomphe::Arc<T>`] is a zero-cost operation
impl<T, S: BackdropStrategy<Box<ArcInner<T>>>> From<Arc<T, S>> for triomphe::Arc<T> {
    #[inline]
    fn from(arc: Arc<T, S>) -> Self {
        unsafe { core::mem::transmute(arc) }
    }
}

impl<A, S: BackdropStrategy<Box<[A]>>> FromIterator<A> for Arc<[A], S>
where
    S: BackdropStrategy<Box<ArcInner<[A]>>>,
    S: BackdropStrategy<Box<ArcInner<HeaderSlice<(), [A]>>>>,
{
    fn from_iter<T: IntoIterator<Item = A>>(iter: T) -> Self {
        UniqueArc::from_iter(iter).shareable()
    }
}

impl<T: ?Sized, S> borrow::Borrow<T> for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    #[inline]
    fn borrow(&self) -> &T {
        &**self
    }
}

impl<T: ?Sized, S> AsRef<T> for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    #[inline]
    fn as_ref(&self) -> &T {
        &**self
    }
}

#[cfg(feature = "stable_deref_trait")]
unsafe impl<T: ?Sized, S> StableDeref for Arc<T, S> where S: BackdropStrategy<Box<ArcInner<T>>> {}
#[cfg(feature = "stable_deref_trait")]
unsafe impl<T: ?Sized, S> CloneStableDeref for Arc<T, S> where S: BackdropStrategy<Box<ArcInner<T>>> {}

#[cfg(feature = "serde")]
impl<'de, T: Deserialize<'de>, S> Deserialize<'de> for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    fn deserialize<D>(deserializer: D) -> Result<Arc<T, S>, D::Error>
    where
        D: ::serde::de::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Arc::new)
    }
}

#[cfg(feature = "serde")]
impl<T: Serialize, S> Serialize for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: ::serde::ser::Serializer,
    {
        (**self).serialize(serializer)
    }
}

#[cfg(feature = "yoke")]
unsafe impl<T, S> yoke::CloneableCart for Arc<T, S> where S: BackdropStrategy<Box<ArcInner<T>>> {}

// Safety:
// This implementation must guarantee that it is sound to call replace_ptr with an unsized variant
// of the pointer retuned in `as_sized_ptr`. The basic property of Unsize coercion is that safety
// variants and layout is unaffected. The Arc does not rely on any other property of T. This makes
// any unsized ArcInner valid for being shared with the sized variant.
// This does _not_ mean that any T can be unsized into an U, but rather than if such unsizing is
// possible then it can be propagated into the Arc<T, S>.
#[cfg(feature = "unsize")]
unsafe impl<T, U: ?Sized, S> unsize::CoerciblePtr<U> for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
    S: BackdropStrategy<Box<ArcInner<U>>>,
{
    type Pointee = T;
    type Output = Arc<U, S>;

    fn as_sized_ptr(&mut self) -> *mut T {
        // Returns a pointer to the complete inner. The unsizing itself won't care about the
        // pointer value and promises not to offset it.
        self.p.as_ptr() as *mut T
    }

    unsafe fn replace_ptr(self, new: *mut U) -> Arc<U, S> {
        // Fix the provenance by ensuring that of `self` is used.
        let inner = ManuallyDrop::new(self);
        let p = inner.p.as_ptr() as *mut T;
        // Safety: This points to an ArcInner of the previous self and holds shared ownership since
        // the old pointer never decremented the reference count. The caller upholds that `new` is
        // an unsized version of the previous ArcInner. This assumes that unsizing to the fat
        // pointer tag of an `ArcInner<U>` and `U` is isomorphic under a direct pointer cast since
        // in reality we unsized *mut T to *mut U at the address of the ArcInner. This is the case
        // for all currently envisioned unsized types where the tag of T and ArcInner<T> are simply
        // the same.
        Arc::from_raw_inner(p.replace_ptr(new) as *mut ArcInner<U>)
    }
}

#[track_caller]
fn must_be_unique<T: ?Sized, S>(arc: &mut Arc<T, S>) -> &mut UniqueArc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    match Arc::try_as_unique(arc) {
        Ok(unique) => unique,
        Err(this) => panic!("`Arc` must be unique in order for this operation to be safe, there are currently {} copies", Arc::count(this)),
    }
}

#[cfg(test)]
mod tests {
    use super::backdrop::TrivialStrategy;
    use crate::arc::Arc;
    use alloc::borrow::ToOwned;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::iter::FromIterator;
    use core::mem::MaybeUninit;
    #[cfg(feature = "unsize")]
    use unsize::{CoerceUnsize, Coercion};

    #[test]
    fn try_unwrap() {
        let x = Arc::<_, TrivialStrategy>::new(100usize);
        let y = x.clone();

        // The count should be two so `try_unwrap()` should fail
        assert_eq!(Arc::count(&x), 2);
        assert!(Arc::try_unwrap(x).is_err());

        // Since `x` has now been dropped, the count should be 1
        // and `try_unwrap()` should succeed
        assert_eq!(Arc::count(&y), 1);
        assert_eq!(Arc::try_unwrap(y), Ok(100));
    }

    #[test]
    #[cfg(feature = "unsize")]
    fn coerce_to_slice() {
        let x = Arc::new([0u8; 4]);
        let y: Arc<[u8], TrivialStrategy> = x.clone().unsize(Coercion::to_slice());
        assert_eq!((*x).as_ptr(), (*y).as_ptr());
    }

    #[test]
    #[cfg(feature = "unsize")]
    fn coerce_to_dyn() {
        let x: Arc<_, TrivialStrategy> = Arc::new(|| 42u32);
        let x: Arc<_, TrivialStrategy> = x.unsize(Coercion::<_, dyn Fn() -> u32>::to_fn());
        assert_eq!((*x)(), 42);
    }

    #[test]
    #[allow(deprecated)]
    fn maybeuninit() {
        let mut arc: Arc<MaybeUninit<_>, TrivialStrategy> = Arc::new_uninit();
        arc.write(999);

        let arc = unsafe { arc.assume_init() };
        assert_eq!(*arc, 999);
    }

    #[test]
    #[allow(deprecated)]
    #[should_panic = "`Arc` must be unique in order for this operation to be safe"]
    fn maybeuninit_ub_to_proceed() {
        let mut uninit = Arc::<_, TrivialStrategy>::new_uninit();
        let clone = uninit.clone();

        let x: &MaybeUninit<String> = &*clone;

        // This write invalidates `x` reference
        uninit.write(String::from("nonononono"));

        // Read invalidated reference to trigger UB
        let _ = &*x;
    }

    #[test]
    #[allow(deprecated)]
    #[should_panic = "`Arc` must be unique in order for this operation to be safe"]
    fn maybeuninit_slice_ub_to_proceed() {
        let mut uninit = Arc::<_, TrivialStrategy>::new_uninit_slice(13);
        let clone = uninit.clone();

        let x: &[MaybeUninit<String>] = &*clone;

        // This write invalidates `x` reference
        uninit.as_mut_slice()[0].write(String::from("nonononono"));

        // Read invalidated reference to trigger UB
        let _ = &*x;
    }

    #[test]
    fn maybeuninit_array() {
        let mut arc: Arc<[MaybeUninit<_>], TrivialStrategy> = Arc::new_uninit_slice(5);
        assert!(arc.is_unique());
        #[allow(deprecated)]
        for (uninit, index) in arc.as_mut_slice().iter_mut().zip(0..5) {
            let ptr = uninit.as_mut_ptr();
            unsafe { core::ptr::write(ptr, index) };
        }

        let arc = unsafe { arc.assume_init() };
        assert!(arc.is_unique());
        // Using clone to that the layout generated in new_uninit_slice is compatible
        // with ArcInner.
        let arcs = [
            arc.clone(),
            arc.clone(),
            arc.clone(),
            arc.clone(),
            arc.clone(),
        ];
        assert_eq!(6, Arc::count(&arc));
        // If the layout is not compatible, then the data might be corrupted.
        assert_eq!(*arc, [0, 1, 2, 3, 4]);

        // Drop the arcs and check the count and the content to
        // make sure it isn't corrupted.
        drop(arcs);
        assert!(arc.is_unique());
        assert_eq!(*arc, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn roundtrip() {
        let arc: Arc<usize, TrivialStrategy> = Arc::new(0usize);
        let ptr = Arc::into_raw(arc);
        unsafe {
            let _arc = Arc::<_, TrivialStrategy>::from_raw(ptr);
        }
    }

    #[test]
    fn from_iterator_exact_size() {
        let arc = Arc::<_, TrivialStrategy>::from_iter(Vec::from_iter([
            "ololo".to_owned(),
            "trololo".to_owned(),
        ]));
        assert_eq!(1, Arc::count(&arc));
        assert_eq!(["ololo".to_owned(), "trololo".to_owned()], *arc);
    }

    #[test]
    fn from_iterator_unknown_size() {
        let arc = Arc::<_, TrivialStrategy>::from_iter(
            Vec::from_iter(["ololo".to_owned(), "trololo".to_owned()])
                .into_iter()
                // Filter is opaque to iterators, so the resulting iterator
                // will report lower bound of 0.
                .filter(|_| true),
        );
        assert_eq!(1, Arc::count(&arc));
        assert_eq!(["ololo".to_owned(), "trololo".to_owned()], *arc);
    }

    #[test]
    fn roundtrip_slice() {
        let arc = Arc::<_, TrivialStrategy>::from(Vec::from_iter([17, 19]));
        let ptr = Arc::<_, TrivialStrategy>::into_raw(arc);
        let arc = unsafe { Arc::<_, TrivialStrategy>::from_raw_slice(ptr) };
        assert_eq!([17, 19], *arc);
        assert_eq!(1, Arc::count(&arc));
    }
}
