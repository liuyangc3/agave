// Module for cuda-related helper functions and wrappers.
//
// cudaHostRegister/cudaHostUnregister -
//    apis for page-pinning memory. Cuda driver/hardware cannot overlap
//    copies from host memory to GPU memory unless the memory is page-pinned and
//    cannot be paged to disk. The cuda driver provides these interfaces to pin and unpin memory.

use {
    crate::{
        perf_libs,
        recycler::{RecyclerX, Reset},
    },
    rand::{seq::SliceRandom, Rng},
    rayon::prelude::*,
    serde::{Deserialize, Serialize},
    std::{
        ops::{Deref, DerefMut, Index, IndexMut},
        os::raw::c_int,
        slice::{Iter, SliceIndex},
        sync::Weak,
    },
};

const CUDA_SUCCESS: c_int = 0;

fn pin<T>(mem: &mut Vec<T>) {
    if let Some(api) = perf_libs::api() {
        use std::{ffi::c_void, mem::size_of};

        let ptr = mem.as_mut_ptr();
        let size = mem.capacity().saturating_mul(size_of::<T>());
        let err = unsafe {
            (api.cuda_host_register)(ptr as *mut c_void, size, /*flags=*/ 0)
        };
        assert!(
            err == CUDA_SUCCESS,
            "cudaHostRegister error: {err} ptr: {ptr:?} bytes: {size}"
        );
    }
}

fn unpin<T>(mem: *mut T) {
    if let Some(api) = perf_libs::api() {
        use std::ffi::c_void;

        let err = unsafe { (api.cuda_host_unregister)(mem as *mut c_void) };
        assert!(
            err == CUDA_SUCCESS,
            "cudaHostUnregister returned: {err} ptr: {mem:?}"
        );
    }
}

// A vector wrapper where the underlying memory can be
// page-pinned. Controlled by flags in case user only wants
// to pin in certain circumstances.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PinnedVec<T: Default + Clone + Sized> {
    x: Vec<T>,
    pinned: bool,
    pinnable: bool,
    #[serde(skip)]
    recycler: Weak<RecyclerX<PinnedVec<T>>>,
}

impl<T: Default + Clone + Sized> Reset for PinnedVec<T> {
    fn reset(&mut self) {
        self.resize(0, T::default());
    }
    fn warm(&mut self, size_hint: usize) {
        self.set_pinnable();
        self.resize(size_hint, T::default());
    }
    fn set_recycler(&mut self, recycler: Weak<RecyclerX<Self>>) {
        self.recycler = recycler;
    }
}

impl<T: Clone + Default + Sized> From<PinnedVec<T>> for Vec<T> {
    fn from(mut pinned_vec: PinnedVec<T>) -> Self {
        if pinned_vec.pinned {
            // If the vector is pinned and has a recycler, just return a clone
            // so that the next allocation of a PinnedVec will recycle an
            // already pinned one.
            if pinned_vec.recycler.strong_count() != 0 {
                return pinned_vec.x.clone();
            }
            unpin(pinned_vec.x.as_mut_ptr());
            pinned_vec.pinned = false;
        }
        pinned_vec.pinnable = false;
        pinned_vec.recycler = Weak::default();
        std::mem::take(&mut pinned_vec.x)
    }
}

impl<'a, T: Clone + Default + Sized> IntoIterator for &'a PinnedVec<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.x.iter()
    }
}

impl<T: Clone + Default + Sized, I: SliceIndex<[T]>> Index<I> for PinnedVec<T> {
    type Output = I::Output;

    #[inline]
    fn index(&self, index: I) -> &Self::Output {
        &self.x[index]
    }
}

impl<T: Clone + Default + Sized, I: SliceIndex<[T]>> IndexMut<I> for PinnedVec<T> {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        &mut self.x[index]
    }
}

impl<'a, T: Clone + Send + Sync + Default + Sized> IntoParallelIterator for &'a PinnedVec<T> {
    type Iter = rayon::slice::Iter<'a, T>;
    type Item = &'a T;
    fn into_par_iter(self) -> Self::Iter {
        self.x.par_iter()
    }
}

impl<'a, T: Clone + Send + Sync + Default + Sized> IntoParallelIterator for &'a mut PinnedVec<T> {
    type Iter = rayon::slice::IterMut<'a, T>;
    type Item = &'a mut T;
    fn into_par_iter(self) -> Self::Iter {
        self.x.par_iter_mut()
    }
}

impl<T: Clone + Default + Sized> PinnedVec<T> {
    pub fn reserve_and_pin(&mut self, size: usize) {
        if self.x.capacity() < size {
            if self.pinned {
                unpin(self.x.as_mut_ptr());
                self.pinned = false;
            }
            self.x.reserve(size);
        }
        self.set_pinnable();
        if !self.pinned {
            pin(&mut self.x);
            self.pinned = true;
        }
    }

    pub fn set_pinnable(&mut self) {
        self.pinnable = true;
    }

    pub fn from_vec(source: Vec<T>) -> Self {
        Self {
            x: source,
            pinned: false,
            pinnable: false,
            recycler: Weak::default(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self::from_vec(Vec::with_capacity(capacity))
    }

    fn prepare_realloc(&mut self, new_size: usize) -> (*mut T, usize) {
        let old_ptr = self.x.as_mut_ptr();
        let old_capacity = self.x.capacity();
        // Predict realloc and unpin.
        if self.pinned && self.x.capacity() < new_size {
            unpin(old_ptr);
            self.pinned = false;
        }
        (old_ptr, old_capacity)
    }

    pub fn push(&mut self, x: T) {
        let (old_ptr, old_capacity) = self.prepare_realloc(self.x.len().saturating_add(1));
        self.x.push(x);
        self.check_ptr(old_ptr, old_capacity, "push");
    }

    pub fn resize(&mut self, size: usize, elem: T) {
        let (old_ptr, old_capacity) = self.prepare_realloc(size);
        self.x.resize(size, elem);
        self.check_ptr(old_ptr, old_capacity, "resize");
    }

    pub fn append(&mut self, other: &mut Vec<T>) {
        let (old_ptr, old_capacity) =
            self.prepare_realloc(self.x.len().saturating_add(other.len()));
        self.x.append(other);
        self.check_ptr(old_ptr, old_capacity, "resize");
    }

    pub fn append_pinned(&mut self, other: &mut Self) {
        let (old_ptr, old_capacity) =
            self.prepare_realloc(self.x.len().saturating_add(other.len()));
        self.x.append(&mut other.x);
        self.check_ptr(old_ptr, old_capacity, "resize");
    }

    pub fn shuffle<R: Rng>(&mut self, rng: &mut R) {
        self.x.shuffle(rng)
    }

    fn check_ptr(&mut self, old_ptr: *mut T, old_capacity: usize, from: &'static str) {
        let api = perf_libs::api();
        if api.is_some()
            && self.pinnable
            && (!std::ptr::eq(self.x.as_ptr(), old_ptr) || self.x.capacity() != old_capacity)
        {
            if self.pinned {
                unpin(old_ptr);
            }

            trace!(
                "pinning from check_ptr old: {} size: {} from: {}",
                old_capacity,
                self.x.capacity(),
                from
            );
            pin(&mut self.x);
            self.pinned = true;
        }
    }
}

impl<T: Clone + Default + Sized> Clone for PinnedVec<T> {
    fn clone(&self) -> Self {
        let mut x = self.x.clone();
        let pinned = if self.pinned {
            pin(&mut x);
            true
        } else {
            false
        };
        debug!(
            "clone PinnedVec: size: {} pinned?: {} pinnable?: {}",
            self.x.capacity(),
            self.pinned,
            self.pinnable
        );
        Self {
            x,
            pinned,
            pinnable: self.pinnable,
            recycler: self.recycler.clone(),
        }
    }
}

impl<T: Sized + Default + Clone> Deref for PinnedVec<T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.x
    }
}

impl<T: Sized + Default + Clone> DerefMut for PinnedVec<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.x
    }
}

impl<T: Sized + Default + Clone> Drop for PinnedVec<T> {
    fn drop(&mut self) {
        if let Some(recycler) = self.recycler.upgrade() {
            recycler.recycle(std::mem::take(self));
        } else if self.pinned {
            unpin(self.x.as_mut_ptr());
        }
    }
}

impl<T: Sized + Default + Clone + PartialEq> PartialEq for PinnedVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.x.eq(&other.x)
    }
}

impl<T: Sized + Default + Clone + PartialEq + Eq> Eq for PinnedVec<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pinned_vec() {
        let mut mem = PinnedVec::with_capacity(10);
        mem.set_pinnable();
        mem.push(50);
        mem.resize(2, 10);
        assert_eq!(mem[0], 50);
        assert_eq!(mem[1], 10);
        assert_eq!(mem.len(), 2);
        assert!(!mem.is_empty());
        let mut iter = mem.iter();
        assert_eq!(*iter.next().unwrap(), 50);
        assert_eq!(*iter.next().unwrap(), 10);
        assert_eq!(iter.next(), None);
    }
}
