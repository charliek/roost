//! Owned CoreFoundation references and the conversions this helper needs.

use super::ffi::*;
use std::ffi::{c_char, c_void};
use std::ptr::NonNull;

/// One +1 reference (from a Create/Copy call, or retained here), released on
/// drop.
pub struct Cf(NonNull<c_void>);

impl Cf {
    /// Adopt a reference the caller owns (a Create or Copy result).
    ///
    /// # Safety
    /// `ptr` is null or a CF object the caller holds a +1 reference to.
    pub unsafe fn owned(ptr: *const c_void) -> Option<Cf> {
        NonNull::new(ptr.cast_mut()).map(Cf)
    }

    /// Retain a borrowed reference (a Get result, an array element).
    ///
    /// # Safety
    /// `ptr` is null or a live CF object.
    pub unsafe fn retained(ptr: *const c_void) -> Option<Cf> {
        let ptr = NonNull::new(ptr.cast_mut())?;
        // SAFETY: the caller guarantees `ptr` is a live CF object.
        unsafe { CFRetain(ptr.as_ptr()) };
        Some(Cf(ptr))
    }

    pub fn ptr(&self) -> *const c_void {
        self.0.as_ptr()
    }

    pub fn type_id(&self) -> CFTypeID {
        // SAFETY: `self` is a live CF object.
        unsafe { CFGetTypeID(self.ptr()) }
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: `self` owns exactly one reference.
        unsafe { CFRelease(self.ptr()) }
    }
}

pub fn string(text: &str) -> Cf {
    // SAFETY: the bytes are valid UTF-8 for the call's duration.
    unsafe {
        Cf::owned(CFStringCreateWithBytes(
            std::ptr::null(),
            text.as_ptr(),
            text.len() as CFIndex,
            kCFStringEncodingUTF8,
            0,
        ))
    }
    .expect("CFStringCreateWithBytes failed on valid UTF-8")
}

/// The Rust string of a CFString, or `None` for anything else.
///
/// # Safety
/// `ptr` is null or a live CF object.
pub unsafe fn to_string(ptr: *const c_void) -> Option<String> {
    // SAFETY: the caller guarantees `ptr` is null or live.
    unsafe {
        if ptr.is_null() || CFGetTypeID(ptr) != CFStringGetTypeID() {
            return None;
        }
        let length = CFStringGetLength(ptr);
        let capacity = CFStringGetMaximumSizeForEncoding(length, kCFStringEncodingUTF8) + 1;
        let mut buffer = vec![0u8; capacity as usize];
        if CFStringGetCString(
            ptr,
            buffer.as_mut_ptr().cast::<c_char>(),
            capacity,
            kCFStringEncodingUTF8,
        ) == 0
        {
            return None;
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        buffer.truncate(end);
        String::from_utf8(buffer).ok()
    }
}

/// A CFNumber as `i64`, or `None` for anything else.
///
/// # Safety
/// `ptr` is null or a live CF object.
pub unsafe fn to_i64(ptr: *const c_void) -> Option<i64> {
    // SAFETY: the caller guarantees `ptr` is null or live.
    unsafe {
        if ptr.is_null() || CFGetTypeID(ptr) != CFNumberGetTypeID() {
            return None;
        }
        let mut value = 0i64;
        (CFNumberGetValue(ptr, kCFNumberSInt64Type, (&mut value as *mut i64).cast()) != 0)
            .then_some(value)
    }
}

/// A CFBoolean, or a CFNumber read as non-zero (the session dictionaries use
/// both for the same keys across releases).
///
/// # Safety
/// `ptr` is null or a live CF object.
pub unsafe fn to_bool(ptr: *const c_void) -> Option<bool> {
    // SAFETY: the caller guarantees `ptr` is null or live.
    unsafe {
        if ptr.is_null() {
            return None;
        }
        if CFGetTypeID(ptr) == CFBooleanGetTypeID() {
            return Some(CFBooleanGetValue(ptr) != 0);
        }
        to_i64(ptr).map(|value| value != 0)
    }
}

/// The value under a string key of a CFDictionary, borrowed from it.
///
/// # Safety
/// `dict` is a live CFDictionary.
pub unsafe fn dict_get(dict: *const c_void, key: &str) -> *const c_void {
    let key = string(key);
    // SAFETY: the caller guarantees `dict` is a live dictionary.
    unsafe { CFDictionaryGetValue(dict, key.ptr()) }
}

/// The elements of a CFArray, each retained.
///
/// # Safety
/// `array` is null or a live CF object.
pub unsafe fn array_items(array: *const c_void) -> Vec<Cf> {
    // SAFETY: the caller guarantees `array` is null or live.
    unsafe {
        if array.is_null() || CFGetTypeID(array) != CFArrayGetTypeID() {
            return Vec::new();
        }
        (0..CFArrayGetCount(array))
            .filter_map(|index| Cf::retained(CFArrayGetValueAtIndex(array, index)))
            .collect()
    }
}

/// Whether `ptr` is a CFDictionary.
///
/// # Safety
/// `ptr` is null or a live CF object.
pub unsafe fn is_dict(ptr: *const c_void) -> bool {
    // SAFETY: the caller guarantees `ptr` is null or live.
    unsafe { !ptr.is_null() && CFGetTypeID(ptr) == CFDictionaryGetTypeID() }
}
