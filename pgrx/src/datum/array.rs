/*
Portions Copyright 2019-2021 ZomboDB, LLC.
Portions Copyright 2021-2022 Technology Concepts & Design, Inc. <support@tcdi.com>

All rights reserved.

Use of this source code is governed by the MIT license that can be found in the LICENSE file.
*/

use crate::array::RawArray;
use crate::layout::*;
use crate::slice::PallocSlice;
use crate::toast::Toast;
use crate::varlena;
use crate::{pg_sys, FromDatum, IntoDatum, PgMemoryContexts};
use bitvec::slice::BitSlice;
use core::ffi::CStr;
use core::ops::DerefMut;
use core::ptr::NonNull;
use once_cell::sync::OnceCell;
use pgrx_pg_sys::Datum;
use pgrx_sql_entity_graph::metadata::{
    ArgumentError, Returns, ReturnsError, SqlMapping, SqlTranslatable,
};
use serde::Serializer;
use std::marker::PhantomData;

/** An array of some type (eg. `TEXT[]`, `int[]`)

While conceptually similar to a [`Vec<T>`][std::vec::Vec], arrays are lazy.

Using a [`Vec<T>`][std::vec::Vec] here means each element of the passed array will be eagerly fetched and converted into a Rust type:

```rust,no_run
use pgrx::prelude::*;

#[pg_extern]
fn with_vec(elems: Vec<String>) {
    // Elements all already converted.
    for elem in elems {
        todo!()
    }
}
```

Using an array, elements are only fetched and converted into a Rust type on demand:

```rust,no_run
use pgrx::prelude::*;

#[pg_extern]
fn with_vec(elems: Array<String>) {
    // Elements converted one by one
    for maybe_elem in elems {
        let elem = maybe_elem.unwrap();
        todo!()
    }
}
```
*/
pub struct Array<'a, T: FromDatum> {
    // Remove this field if/when we figure out how to stop using pg_sys::deconstruct_array
    null_slice: NullKind<'a>,
    elem_layout: Layout,
    _datum_slice: OnceCell<PallocSlice<pg_sys::Datum>>,
    // Rust drops in FIFO order, drop this last
    raw: Toast<RawArray>,
    _marker: PhantomData<T>,
}

enum NullKind<'a> {
    Bits(&'a BitSlice<u8>),
    Strict(usize),
}

impl NullKind<'_> {
    fn get(&self, index: usize) -> Option<bool> {
        match self {
            // Note this flips the bit:
            // Postgres nullbitmaps are 1 for "valid" and 0 for "null"
            Self::Bits(b1) => b1.get(index).map(|b| !b),
            Self::Strict(len) => index.lt(len).then(|| false),
        }
    }

    fn any(&self) -> bool {
        match self {
            // Note the reversed polarity:
            // Postgres nullbitmaps are 1 for "valid" and 0 for "null"
            Self::Bits(b1) => !b1.all(),
            Self::Strict(_) => false,
        }
    }
}

impl<'a, T: FromDatum + serde::Serialize> serde::Serialize for Array<'a, T> {
    fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
    where
        S: Serializer,
    {
        serializer.collect_seq(self.iter())
    }
}

#[deny(unsafe_op_in_unsafe_fn)]
impl<'a, T: FromDatum> Array<'a, T> {
    /// # Safety
    ///
    /// This function requires that the RawArray was obtained in a properly-constructed form
    /// (probably from Postgres).
    unsafe fn deconstruct_from(mut raw: Toast<RawArray>) -> Array<'a, T> {
        let oid = raw.oid();
        let elem_layout = Layout::lookup_oid(oid);
        let nelems = raw.len();
        let null_slice = raw
            .nulls_bitslice()
            .map(|nonnull| NullKind::Bits(unsafe { &*nonnull.as_ptr() }))
            .unwrap_or(NullKind::Strict(nelems));
        let _datum_slice = OnceCell::new();

        #[cfg(debug_assertions)]
        let Ok(()) = _datum_slice.set(unsafe {
            let (datums, bools) = raw.deconstruct(elem_layout);
            // Don't need this.
            pg_sys::pfree(bools.cast());
            PallocSlice::from_raw_parts(NonNull::new(datums).unwrap(), nelems)
        }) else {
            panic!("oh no, the debug code exploded!")
        };

        // The array-walking code assumes this is always the case, is it?
        if let Layout { size: Size::Fixed(n), align, .. } = elem_layout {
            let n: usize = n.into();
            assert!(
                n % (align.as_usize()) == 0,
                "typlen does NOT include padding for fixed-width layouts!"
            );
        }

        Array { raw, _datum_slice, null_slice, elem_layout, _marker: PhantomData }
    }

    /// Rips out the underlying `pg_sys::ArrayType` pointer.
    /// Note that Array may have caused Postgres to allocate to unbox the datum,
    /// and this can hypothetically cause a memory leak if so.
    pub fn into_array_type(self) -> *const pg_sys::ArrayType {
        // may be worth replacing this function when Toast<T> matures enough
        // to be used as a public type with a fn(self) -> Toast<RawArray>

        let Array { raw, .. } = self;
        // Wrap the Toast<RawArray> to prevent it from deallocating itself
        let mut raw = core::mem::ManuallyDrop::new(raw);
        let ptr = raw.deref_mut().deref_mut() as *mut RawArray;
        // SAFETY: Leaks are safe if they aren't use-after-frees!
        unsafe { ptr.read() }.into_ptr().as_ptr() as _
    }

    /// Return an iterator of `Option<T>`.
    pub fn iter(&self) -> ArrayIterator<'_, T> {
        let ptr = self.raw.data_ptr();
        ArrayIterator { array: self, curr: 0, ptr }
    }

    /// Return an iterator over the Array's elements.
    ///
    /// # Panics
    /// This function will panic when called if the array contains any SQL NULL values.
    pub fn iter_deny_null(&self) -> ArrayTypedIterator<'_, T> {
        if self.null_slice.any() {
            panic!("array contains NULL");
        }

        let ptr = self.raw.data_ptr();
        ArrayTypedIterator { array: self, curr: 0, ptr }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.raw.len() == 0
    }

    #[allow(clippy::option_option)]
    #[inline]
    pub fn get(&self, index: usize) -> Option<Option<T>> {
        let Some(is_null) = self.null_slice.get(index) else { return None };
        if is_null {
            return Some(None);
        }

        // This pointer is what's walked over the entire array's data buffer.
        // If the array has varlena or cstr elements, we can't index into the array.
        // If the elements are fixed size, we could, but we do not exploit that optimization yet
        // as it would significantly complicate the code and impact debugging it.
        // Such improvements should wait until a later version (today's: 0.7.4, preparing 0.8.0).
        let mut at_byte = self.raw.data_ptr();
        for i in 0..index {
            match self.null_slice.get(i) {
                None => unreachable!("array was exceeded while walking to known non-null index???"),
                // Skip nulls: the data buffer has no placeholders for them!
                Some(true) => continue,
                Some(false) => {
                    #[cfg(debug_assertions)]
                    if let PassBy::Ref = self.elem_layout.pass {
                        assert_eq!(
                            Some(pg_sys::Datum::from(at_byte)),
                            self._datum_slice.get().and_then(|s| unsafe { s.get(i) }).copied()
                        );
                    }
                    // SAFETY: Note this entire function has to be correct,
                    // not just this one call, for this to be correct!
                    at_byte = unsafe { self.one_hop_this_time(at_byte, self.elem_layout) };
                }
            }
        }

        // If this has gotten this far, it is known to be non-null,
        // all the null values in the array up to this index were skipped,
        // and the only offsets were via our hopping function.
        Some(unsafe { self.bring_it_back_now(at_byte, index, is_null) })
    }

    /// Extracts an element from a Postgres Array's data buffer
    ///
    /// # Safety
    /// This assumes the pointer is to a valid element of that type.
    #[inline]
    unsafe fn bring_it_back_now(&self, ptr: *const u8, _index: usize, is_null: bool) -> Option<T> {
        if is_null {
            return None;
        }

        match self.elem_layout.pass {
            PassBy::Value => match self.elem_layout.size {
                //
                // NB:  Leaving this commented out because it's not clear to me that this will be
                // correct in every case.  This assumption got us in trouble with arrays of enums
                // already, and I'd rather err on the side of correctness.
                //
                // Size::Fixed(size) if size as usize == std::mem::size_of::<T>() => unsafe {
                //     // short-circuit if the size of the element matches the size of `T`.
                //     // This most likely means that the element Datum actually represents the same
                //     // type as the rust `T`
                //
                //     Some(ptr.cast::<T>().read())
                // },
                Size::Fixed(size) => {
                    // copy off `size` bytes from the head of `ptr` and convert that into a `usize`
                    // using proper platform endianness, converting it into a `Datum`
                    #[inline(always)]
                    fn bytes_to_datum(ptr: *const u8, size: usize) -> Datum {
                        const USIZE_BYTE_LEN: usize = std::mem::size_of::<usize>();

                        // a zero-padded buffer in which we'll store bytes so we can
                        // ultimately make a `usize` that we convert into a `Datum`
                        let mut buf = [0u8; USIZE_BYTE_LEN];

                        match size {
                            1..=USIZE_BYTE_LEN => unsafe {
                                // copy to the end
                                #[cfg(target_endian = "big")]
                                let dst = (&mut buff[8 - size as usize..]).as_mut_ptr();

                                // copy to the head
                                #[cfg(target_endian = "little")]
                                let dst = (&mut buf[0..]).as_mut_ptr();

                                std::ptr::copy_nonoverlapping(ptr, dst, size as usize);
                            },
                            other => {
                                panic!("unexpected fixed size array element size: {}", other)
                            }
                        }

                        Datum::from(usize::from_ne_bytes(buf))
                    }

                    let datum = bytes_to_datum(ptr, size as usize);
                    unsafe { T::from_polymorphic_datum(datum, false, self.raw.oid()) }
                }

                other => {
                    panic!("unrecognized pass-by-value array element layout size: {:?}", other)
                }
            },
            PassBy::Ref => {
                let datum = pg_sys::Datum::from(ptr);
                #[cfg(debug_assertions)]
                assert_eq!(
                    Some(datum),
                    self._datum_slice.get().and_then(|s| unsafe { s.get(_index) }).copied()
                );
                unsafe { T::from_polymorphic_datum(datum, false, self.raw.oid()) }
            }
        }
    }

    /// Walk the data of a Postgres Array, "hopping" according to element layout.
    ///
    /// # Safety
    /// For the varlena/cstring layout, data in the buffer is read.
    /// In either case, pointer arithmetic is done, with the usual implications,
    /// e.g. the pointer must be <= a "one past the end" pointer
    /// This means this function must be invoked with the correct layout, and
    /// either the array's `data_ptr` or a correctly offset pointer into it.
    ///
    /// Null elements will NOT be present in a Postgres Array's data buffer!
    /// Do not cumulatively invoke this more than `len - null_count`!
    /// Doing so will result in reading uninitialized data, which is UB!
    #[inline]
    unsafe fn one_hop_this_time(&self, ptr: *const u8, layout: Layout) -> *const u8 {
        unsafe {
            let offset = match layout {
                Layout { size: Size::Fixed(n), .. } => n.into(),
                Layout { size: Size::Varlena, align, .. } => {
                    // SAFETY: This uses the varsize_any function to be safe,
                    // and the caller was informed of pointer requirements.
                    let varsize = varlena::varsize_any(ptr.cast());

                    // the Postgres realignment code may seem different in form,
                    // but it's the same in function, just micro-optimized
                    let align = align.as_usize();
                    let align_mask = varsize & (align - 1);
                    let align_offset = if align_mask != 0 { align - align_mask } else { 0 };

                    varsize + align_offset
                }
                Layout { size: Size::CStr, .. } => {
                    // TODO: this code is dangerously under-exercised in the test suite
                    // SAFETY: The caller was informed of pointer requirements.
                    let strlen = CStr::from_ptr(ptr.cast()).to_bytes().len();

                    // Skip over the null and into the next cstr!
                    strlen + 2
                }
            };

            // SAFETY: ptr stops at 1-past-end of the array's varlena
            debug_assert!(ptr.wrapping_add(offset) <= self.raw.end_ptr());
            ptr.add(offset)
        }
    }
}

pub struct VariadicArray<'a, T: FromDatum>(Array<'a, T>);

impl<'a, T: FromDatum + serde::Serialize> serde::Serialize for VariadicArray<'a, T> {
    fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
    where
        S: Serializer,
    {
        serializer.collect_seq(self.0.iter())
    }
}

impl<'a, T: FromDatum> VariadicArray<'a, T> {
    pub fn into_array_type(self) -> *const pg_sys::ArrayType {
        self.0.into_array_type()
    }

    /// Return an Iterator of Option<T> over the contained Datums.
    pub fn iter(&self) -> ArrayIterator<'_, T> {
        self.0.iter()
    }

    /// Return an iterator over the Array's elements.
    ///
    /// # Panics
    /// This function will panic when called if the array contains any SQL NULL values.
    pub fn iter_deny_null(&self) -> ArrayTypedIterator<'_, T> {
        self.0.iter_deny_null()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[allow(clippy::option_option)]
    #[inline]
    pub fn get(&self, i: usize) -> Option<Option<T>> {
        self.0.get(i)
    }
}

pub struct ArrayTypedIterator<'a, T: 'a + FromDatum> {
    array: &'a Array<'a, T>,
    curr: usize,
    ptr: *const u8,
}

impl<'a, T: FromDatum> Iterator for ArrayTypedIterator<'a, T> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let Self { array, curr, ptr } = self;
        if *curr >= array.raw.len() {
            None
        } else {
            // SAFETY: The constructor for this type instantly panics if any nulls are present!
            // Thus as an invariant, this will never have to reckon with the nullbitmap.
            let element = unsafe { array.bring_it_back_now(*ptr, *curr, false) };
            *curr += 1;
            *ptr = unsafe { array.one_hop_this_time(*ptr, array.elem_layout) };
            element
        }
    }
}

impl<'a, T: FromDatum + serde::Serialize> serde::Serialize for ArrayTypedIterator<'a, T> {
    fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
    where
        S: Serializer,
    {
        serializer.collect_seq(self.array.iter())
    }
}

pub struct ArrayIterator<'a, T: 'a + FromDatum> {
    array: &'a Array<'a, T>,
    curr: usize,
    ptr: *const u8,
}

impl<'a, T: FromDatum> Iterator for ArrayIterator<'a, T> {
    type Item = Option<T>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let Self { array, curr, ptr } = self;
        let Some(is_null) = array.null_slice.get(*curr) else { return None };
        let element = unsafe { array.bring_it_back_now(*ptr, *curr, is_null) };
        *curr += 1;
        if let Some(false) = array.null_slice.get(*curr) {
            *ptr = unsafe { array.one_hop_this_time(*ptr, array.elem_layout) };
        }
        Some(element)
    }
}

pub struct ArrayIntoIterator<'a, T: FromDatum> {
    array: Array<'a, T>,
    curr: usize,
    ptr: *const u8,
}

impl<'a, T: FromDatum> IntoIterator for Array<'a, T> {
    type Item = Option<T>;
    type IntoIter = ArrayIntoIterator<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        let ptr = self.raw.data_ptr();
        ArrayIntoIterator { array: self, curr: 0, ptr }
    }
}

impl<'a, T: FromDatum> IntoIterator for VariadicArray<'a, T> {
    type Item = Option<T>;
    type IntoIter = ArrayIntoIterator<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        let ptr = self.0.raw.data_ptr();
        ArrayIntoIterator { array: self.0, curr: 0, ptr }
    }
}

impl<'a, T: FromDatum> Iterator for ArrayIntoIterator<'a, T> {
    type Item = Option<T>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let Self { array, curr, ptr } = self;
        let Some(is_null) = array.null_slice.get(*curr) else { return None };
        let element = unsafe { array.bring_it_back_now(*ptr, *curr, is_null) };
        *curr += 1;
        if let Some(false) = array.null_slice.get(*curr) {
            *ptr = unsafe { array.one_hop_this_time(*ptr, array.elem_layout) };
        }
        Some(element)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // If asking for size, it's not clear if they want "actual size"
        // or "size minus nulls"? Let's lower bound on 0 if nulls exist.
        let left = self.array.raw.len() - self.curr;
        if let NullKind::Strict(_) = self.array.null_slice {
            (left, Some(left))
        } else {
            (0, Some(left))
        }
    }

    fn count(self) -> usize {
        // TODO: This code is dangerously under-exercised in the test suite.
        self.array.raw.len() - self.curr
    }
}

impl<'a, T: FromDatum> FromDatum for VariadicArray<'a, T> {
    #[inline]
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        oid: pg_sys::Oid,
    ) -> Option<VariadicArray<'a, T>> {
        Array::from_polymorphic_datum(datum, is_null, oid).map(Self)
    }
}

impl<'a, T: FromDatum> FromDatum for Array<'a, T> {
    #[inline]
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        _typoid: pg_sys::Oid,
    ) -> Option<Array<'a, T>> {
        if is_null {
            None
        } else {
            let Some(ptr) = NonNull::new(datum.cast_mut_ptr()) else { return None };
            let raw = RawArray::detoast_from_varlena(ptr);
            Some(Array::deconstruct_from(raw))
        }
    }

    unsafe fn from_datum_in_memory_context(
        mut memory_context: PgMemoryContexts,
        datum: pg_sys::Datum,
        is_null: bool,
        typoid: pg_sys::Oid,
    ) -> Option<Self>
    where
        Self: Sized,
    {
        if is_null {
            None
        } else {
            memory_context.switch_to(|_| {
                // copy the Datum into this MemoryContext, and then instantiate the Array wrapper
                let copy = pg_sys::pg_detoast_datum_copy(datum.cast_mut_ptr());
                Array::<T>::from_polymorphic_datum(pg_sys::Datum::from(copy), false, typoid)
            })
        }
    }
}

impl<T: FromDatum> FromDatum for Vec<T> {
    #[inline]
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        typoid: pg_sys::Oid,
    ) -> Option<Vec<T>> {
        if is_null {
            None
        } else {
            Array::<T>::from_polymorphic_datum(datum, is_null, typoid)
                .map(|array| array.iter_deny_null().collect::<Vec<_>>())
        }
    }

    unsafe fn from_datum_in_memory_context(
        memory_context: PgMemoryContexts,
        datum: pg_sys::Datum,
        is_null: bool,
        typoid: pg_sys::Oid,
    ) -> Option<Self>
    where
        Self: Sized,
    {
        Array::<T>::from_datum_in_memory_context(memory_context, datum, is_null, typoid)
            .map(|array| array.iter_deny_null().collect::<Vec<_>>())
    }
}

impl<T: FromDatum> FromDatum for Vec<Option<T>> {
    #[inline]
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        typoid: pg_sys::Oid,
    ) -> Option<Vec<Option<T>>> {
        Array::<T>::from_polymorphic_datum(datum, is_null, typoid)
            .map(|array| array.iter().collect::<Vec<_>>())
    }

    unsafe fn from_datum_in_memory_context(
        memory_context: PgMemoryContexts,
        datum: pg_sys::Datum,
        is_null: bool,
        typoid: pg_sys::Oid,
    ) -> Option<Self>
    where
        Self: Sized,
    {
        Array::<T>::from_datum_in_memory_context(memory_context, datum, is_null, typoid)
            .map(|array| array.iter().collect::<Vec<_>>())
    }
}

impl<T> IntoDatum for Vec<T>
where
    T: IntoDatum,
{
    fn into_datum(self) -> Option<pg_sys::Datum> {
        let mut state = unsafe {
            pg_sys::initArrayResult(
                T::type_oid(),
                PgMemoryContexts::CurrentMemoryContext.value(),
                false,
            )
        };
        for s in self {
            let datum = s.into_datum();
            let isnull = datum.is_none();

            unsafe {
                state = pg_sys::accumArrayResult(
                    state,
                    datum.unwrap_or(0.into()),
                    isnull,
                    T::type_oid(),
                    PgMemoryContexts::CurrentMemoryContext.value(),
                );
            }
        }

        if state.is_null() {
            // shouldn't happen
            None
        } else {
            Some(unsafe {
                pg_sys::makeArrayResult(state, PgMemoryContexts::CurrentMemoryContext.value())
            })
        }
    }

    fn type_oid() -> pg_sys::Oid {
        unsafe { pg_sys::get_array_type(T::type_oid()) }
    }

    #[inline]
    fn is_compatible_with(other: pg_sys::Oid) -> bool {
        Self::type_oid() == other || other == unsafe { pg_sys::get_array_type(T::type_oid()) }
    }
}

impl<'a, T> IntoDatum for &'a [T]
where
    T: IntoDatum + Copy + 'a,
{
    fn into_datum(self) -> Option<pg_sys::Datum> {
        let mut state = unsafe {
            pg_sys::initArrayResult(
                T::type_oid(),
                PgMemoryContexts::CurrentMemoryContext.value(),
                false,
            )
        };
        for s in self {
            let datum = s.into_datum();
            let isnull = datum.is_none();

            unsafe {
                state = pg_sys::accumArrayResult(
                    state,
                    datum.unwrap_or(0.into()),
                    isnull,
                    T::type_oid(),
                    PgMemoryContexts::CurrentMemoryContext.value(),
                );
            }
        }

        if state.is_null() {
            // shouldn't happen
            None
        } else {
            Some(unsafe {
                pg_sys::makeArrayResult(state, PgMemoryContexts::CurrentMemoryContext.value())
            })
        }
    }

    fn type_oid() -> pg_sys::Oid {
        unsafe { pg_sys::get_array_type(T::type_oid()) }
    }

    #[inline]
    fn is_compatible_with(other: pg_sys::Oid) -> bool {
        Self::type_oid() == other || other == unsafe { pg_sys::get_array_type(T::type_oid()) }
    }
}

unsafe impl<'a, T> SqlTranslatable for Array<'a, T>
where
    T: SqlTranslatable + FromDatum,
{
    fn argument_sql() -> Result<SqlMapping, ArgumentError> {
        match T::argument_sql()? {
            SqlMapping::As(sql) => Ok(SqlMapping::As(format!("{sql}[]"))),
            SqlMapping::Skip => Err(ArgumentError::SkipInArray),
            SqlMapping::Composite { .. } => Ok(SqlMapping::Composite { array_brackets: true }),
            SqlMapping::Source { .. } => Ok(SqlMapping::Source { array_brackets: true }),
        }
    }

    fn return_sql() -> Result<Returns, ReturnsError> {
        match T::return_sql()? {
            Returns::One(SqlMapping::As(sql)) => {
                Ok(Returns::One(SqlMapping::As(format!("{sql}[]"))))
            }
            Returns::One(SqlMapping::Composite { array_brackets: _ }) => {
                Ok(Returns::One(SqlMapping::Composite { array_brackets: true }))
            }
            Returns::One(SqlMapping::Source { array_brackets: _ }) => {
                Ok(Returns::One(SqlMapping::Source { array_brackets: true }))
            }
            Returns::One(SqlMapping::Skip) => Err(ReturnsError::SkipInArray),
            Returns::SetOf(_) => Err(ReturnsError::SetOfInArray),
            Returns::Table(_) => Err(ReturnsError::TableInArray),
        }
    }
}

unsafe impl<'a, T> SqlTranslatable for VariadicArray<'a, T>
where
    T: SqlTranslatable + FromDatum,
{
    fn argument_sql() -> Result<SqlMapping, ArgumentError> {
        match T::argument_sql()? {
            SqlMapping::As(sql) => Ok(SqlMapping::As(format!("{sql}[]"))),
            SqlMapping::Skip => Err(ArgumentError::SkipInArray),
            SqlMapping::Composite { .. } => Ok(SqlMapping::Composite { array_brackets: true }),
            SqlMapping::Source { .. } => Ok(SqlMapping::Source { array_brackets: true }),
        }
    }

    fn return_sql() -> Result<Returns, ReturnsError> {
        match T::return_sql()? {
            Returns::One(SqlMapping::As(sql)) => {
                Ok(Returns::One(SqlMapping::As(format!("{sql}[]"))))
            }
            Returns::One(SqlMapping::Composite { array_brackets: _ }) => {
                Ok(Returns::One(SqlMapping::Composite { array_brackets: true }))
            }
            Returns::One(SqlMapping::Source { array_brackets: _ }) => {
                Ok(Returns::One(SqlMapping::Source { array_brackets: true }))
            }
            Returns::One(SqlMapping::Skip) => Err(ReturnsError::SkipInArray),
            Returns::SetOf(_) => Err(ReturnsError::SetOfInArray),
            Returns::Table(_) => Err(ReturnsError::TableInArray),
        }
    }

    fn variadic() -> bool {
        true
    }
}
