use std::borrow::Borrow;
use std::cell::{BorrowError, BorrowMutError, RefCell};
use std::cmp::Ordering;
use std::fmt::{self, Debug, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::ops::{Deref, DerefMut};
use std::sync;

use stable_deref_trait::StableDeref;

use crate::collector::{InternalGcRef, COLLECTOR};
use crate::lockout::Warrant;
use crate::Scan;

/// `Gc` is a smart-pointer for data tracked by `shredder` garbage collector
pub struct Gc<T: Scan> {
    backing_handle: InternalGcRef,
    direct_ptr: *const T,
}

impl<T: Scan> Gc<T> {
    /// Create a new `Gc` containing the given data
    pub fn new(v: T) -> Self
    where
        T: 'static,
    {
        let (handle, ptr) = COLLECTOR.track_data(v);
        Self {
            backing_handle: handle,
            direct_ptr: ptr,
        }
    }

    /// `get` is used to get a `GcGuard`. This is usually what you want when accessing non-`Sync`
    /// data in a `Gc`. The API is very analogous to the `Mutex` API. It may block if the data is
    /// being scanned
    #[must_use]
    pub fn get(&self) -> GcGuard<T> {
        let warrant = COLLECTOR.get_data_warrant(&self.backing_handle);
        GcGuard {
            gc_ptr: self,
            _warrant: warrant,
        }
    }

    pub(crate) fn internal_handle(&self) -> InternalGcRef {
        self.backing_handle.clone()
    }
}

impl<T: Scan> Clone for Gc<T> {
    #[must_use]
    fn clone(&self) -> Self {
        let new_handle = COLLECTOR.clone_handle(&self.backing_handle);

        Self {
            backing_handle: new_handle,
            direct_ptr: self.direct_ptr,
        }
    }
}

// Same bounds as Arc<T>
unsafe impl<T: Scan> Sync for Gc<T> where T: Sync + Send {}
unsafe impl<T: Scan> Send for Gc<T> where T: Sync + Send {}
// Since we can clone Gc<T>, being able to send a Gc<T> implies possible sharing between threads
// (Thus for Gc<T> to be send, T must be Send and Sync)

impl<T: Scan> Drop for Gc<T> {
    fn drop(&mut self) {
        // This may trigger during Gc-drop, but it'll do nothing and everything will be fine
        COLLECTOR.drop_handle(&self.backing_handle);
    }
}

// This is special casing for Gc<RefCell<T>>
rental! {
    mod gc_refcell_internals {
        use crate::{Scan, GcGuard};
        use std::cell::{Ref, RefCell, RefMut};

        /// Self referential wrapper around `Ref` for ergonomics
        #[rental(deref_suffix)]
        pub struct GcRefInt<'a, T: Scan + 'static> {
            gc_guard: GcGuard<'a, RefCell<T>>,
            cell_ref: Ref<'gc_guard, T>
        }

        /// Self referential wrapper around `RefMut` for ergonomics
        #[rental(deref_mut_suffix)]
        pub struct GcRefMutInt<'a, T: Scan + 'static> {
            gc_guard: GcGuard<'a, RefCell<T>>,
            cell_ref: RefMut<'gc_guard, T>
        }
    }
}

/// This is like a `Ref`, but taken directly from a `Gc`
pub struct GcRef<'a, T: Scan + 'static> {
    internal_ref: gc_refcell_internals::GcRefInt<'a, T>,
}

impl<'a, T: Scan + 'static> Deref for GcRef<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.internal_ref.deref()
    }
}

/// This is like a `RefMut`, but taken directly from a `Gc`
pub struct GcRefMut<'a, T: Scan + 'static> {
    internal_ref: gc_refcell_internals::GcRefMutInt<'a, T>,
}

impl<'a, T: Scan + 'static> Deref for GcRefMut<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.internal_ref.deref()
    }
}

impl<'a, T: Scan + 'static> DerefMut for GcRefMut<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.internal_ref.deref_mut()
    }
}

impl<T: Scan + 'static> Gc<RefCell<T>> {
    /// Call the underlying `borrow` method on the `RefCell`.
    ///
    /// This is just a nice method so you don't have to call `get` manually.
    #[must_use]
    pub fn borrow(&self) -> GcRef<T> {
        let g = self.get();
        let internal_ref = gc_refcell_internals::GcRefInt::new(g, RefCell::borrow);

        GcRef { internal_ref }
    }

    /// Call the underlying `try_borrow` method on the `RefCell`.
    ///
    /// This is just a nice method so you don't have to call `get` manually.
    ///
    /// # Errors
    /// Propagates a `BorrowError` if the underlying `RefCell` is already borrowed mutably
    pub fn try_borrow(&self) -> Result<GcRef<T>, BorrowError> {
        let g = self.get();
        let internal_ref =
            gc_refcell_internals::GcRefInt::try_new(g, RefCell::try_borrow).map_err(|e| e.0)?;

        Ok(GcRef { internal_ref })
    }

    /// Call the underlying `borrow_mut` method on the `RefCell`.
    ///
    /// This is just a nice method so you don't have to call `get` manually.
    #[must_use]
    pub fn borrow_mut(&self) -> GcRefMut<T> {
        let g = self.get();
        let internal_ref = gc_refcell_internals::GcRefMutInt::new(g, RefCell::borrow_mut);

        GcRefMut { internal_ref }
    }

    /// Call the underlying `try_borrow_mut` method on the `RefCell`.
    ///
    /// This is just a nice method so you don't have to call `get` manually.
    /// # Errors
    /// Propagates a `BorrowError` if the underlying `RefCell` is already borrowed
    pub fn try_borrow_mut(&self) -> Result<GcRefMut<T>, BorrowMutError> {
        let g = self.get();
        let internal_ref = gc_refcell_internals::GcRefMutInt::try_new(g, RefCell::try_borrow_mut)
            .map_err(|e| e.0)?;

        Ok(GcRefMut { internal_ref })
    }
}

// This is special casing for Gc<Mutex<T>>
rental! {
    mod gc_mutex_internals {
        use crate::{Scan, GcGuard};
        use std::sync::{Mutex, MutexGuard};

        /// Self referential wrapper around `MutexGuard` for ergonomics
        #[rental(deref_mut_suffix)]
        pub struct GcMutexGuardInt<'a, T: Scan + 'static> {
            gc_guard: GcGuard<'a, Mutex<T>>,
            cell_ref: MutexGuard<'gc_guard, T>
        }
    }
}

/// This is like a `MutexGuard`, but taken directly from a `Gc`
pub struct GcMutexGuard<'a, T: Scan + 'static> {
    internal_guard: gc_mutex_internals::GcMutexGuardInt<'a, T>,
}

impl<T: Scan + 'static> Deref for GcMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.internal_guard.deref()
    }
}

impl<T: Scan + 'static> DerefMut for GcMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.internal_guard.deref_mut()
    }
}

// TODO: Give users a way to recover from poison without manual intervention
#[derive(Debug)]
pub struct GcMutexPoisonError;

impl<T: Scan + 'static> Gc<sync::Mutex<T>> {
    /// Call the underlying lock method on the inner `Mutex`
    ///
    /// This is just a nice method so you don't have to `get` manually
    ///
    /// # Errors
    /// Returns a `GcMutexPoisonError` if the underlying `.lock` method returns an error
    /// Note that this error, unlike the underlying one, does not give a way to recover the guard
    pub fn lock(&self) -> Result<GcMutexGuard<T>, GcMutexPoisonError> {
        let g = self.get();
        let internal_guard = gc_mutex_internals::GcMutexGuardInt::try_new(g, |g| match g.lock() {
            Ok(v) => Ok(v),
            Err(_) => Err(GcMutexPoisonError),
        })
        .map_err(|e| e.0)?;

        Ok(GcMutexGuard { internal_guard })
    }
}

// Lots of traits it's good for a smart ptr to implement:
impl<T: Scan> Debug for Gc<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gc")
            .field("backing_handle", &"<SNIP>")
            .field("direct_ptr", &self.direct_ptr)
            .finish()
    }
}

impl<T: Scan> Default for Gc<T>
where
    T: Default + 'static,
{
    #[must_use]
    fn default() -> Self {
        let v = T::default();
        Self::new(v)
    }
}

impl<T: Scan> Display for Gc<T>
where
    T: Display,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let a = self.get();
        a.fmt(f)
    }
}

impl<T: Scan> fmt::Pointer for Gc<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.direct_ptr, f)
    }
}

impl<T: Scan> Eq for Gc<T> where T: Eq {}

impl<T: Scan> Hash for Gc<T>
where
    T: Hash,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.get().hash(state)
    }
}

impl<T: Scan> Ord for Gc<T>
where
    T: Ord,
{
    #[must_use]
    fn cmp(&self, other: &Self) -> Ordering {
        let a = self.get();
        let b = other.get();

        a.cmp(b.deref())
    }
}

#[allow(clippy::partialeq_ne_impl)]
impl<T: Scan> PartialEq for Gc<T>
where
    T: PartialEq,
{
    #[must_use]
    fn eq(&self, other: &Self) -> bool {
        let a = self.get();
        let b = other.get();
        a.eq(&b)
    }

    #[must_use]
    fn ne(&self, other: &Self) -> bool {
        let a = self.get();
        let b = other.get();
        a.ne(&b)
    }
}

impl<T: Scan> PartialOrd for Gc<T>
where
    T: PartialOrd,
{
    #[must_use]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let a = self.get();
        let b = other.get();

        a.partial_cmp(&b)
    }

    #[must_use]
    fn lt(&self, other: &Self) -> bool {
        let a = self.get();
        let b = other.get();

        a.lt(&b)
    }

    #[must_use]
    fn le(&self, other: &Self) -> bool {
        let a = self.get();
        let b = other.get();

        a.le(&b)
    }

    #[must_use]
    fn gt(&self, other: &Self) -> bool {
        let a = self.get();
        let b = other.get();

        a.gt(&b)
    }

    #[must_use]
    fn ge(&self, other: &Self) -> bool {
        let a = self.get();
        let b = other.get();

        a.ge(&b)
    }
}

/// A `GcGuard` lets you access the underlying data of a `Gc`
/// It exists as data needs protection from being scanned while it's being concurrently modified
pub struct GcGuard<'a, T: Scan> {
    gc_ptr: &'a Gc<T>,
    _warrant: Warrant,
}

impl<'a, T: Scan> Deref for GcGuard<'a, T> {
    type Target = T;

    #[must_use]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.gc_ptr.direct_ptr }
    }
}

/// It is impossible for the value behind a `GcGuard` to move (since it's basically a `&T`)
unsafe impl<'a, T: Scan> StableDeref for GcGuard<'a, T> {}

impl<'a, T: Scan> AsRef<T> for GcGuard<'a, T> {
    #[must_use]
    fn as_ref(&self) -> &T {
        self.deref()
    }
}

impl<'a, T: Scan> Borrow<T> for GcGuard<'a, T> {
    #[must_use]
    fn borrow(&self) -> &T {
        self.deref()
    }
}

impl<'a, T: Scan + Debug> Debug for GcGuard<'a, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcGuard")
            .field("v", self.deref())
            .field("warrant", &"<SNIP>")
            .finish()
    }
}
