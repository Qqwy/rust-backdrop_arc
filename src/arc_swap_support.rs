use super::backdrop::BackdropStrategy;
use arc_swap::RefCnt;

use crate::arc::ArcInner;
use crate::Arc;

unsafe impl<T, S> RefCnt for Arc<T, S>
where
    S: BackdropStrategy<Box<ArcInner<T>>>,
{
    type Base = T;

    #[inline]
    fn into_ptr(me: Self) -> *mut Self::Base {
        Arc::into_raw(me) as *mut _
    }

    #[inline]
    fn as_ptr(me: &Self) -> *mut Self::Base {
        Arc::as_ptr(me) as *mut _
    }

    #[inline]
    unsafe fn from_ptr(ptr: *const Self::Base) -> Self {
        Arc::from_raw(ptr)
    }
}

// For now do not support ThinArc
// use crate::{Arc, ThinArc};
// use core::ffi::c_void;

// unsafe impl<H, T> RefCnt for ThinArc<H, T> {
//     type Base = c_void;

//     #[inline]
//     fn into_ptr(me: Self) -> *mut Self::Base {
//         ThinArc::into_raw(me) as *mut _
//     }

//     #[inline]
//     fn as_ptr(me: &Self) -> *mut Self::Base {
//         ThinArc::as_ptr(me) as *mut _
//     }

//     #[inline]
//     unsafe fn from_ptr(ptr: *const Self::Base) -> Self {
//         ThinArc::from_raw(ptr)
//     }
// }
