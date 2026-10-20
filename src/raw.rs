//! `RawJsonb`: a `jsonb` argument passed to Rust as its detoasted bytes, so the
//! validator can walk the on-disk `JsonbContainer` without building a
//! `serde_json::Value`.

use pgrx::callconv::{Arg, ArgAbi};
use pgrx::prelude::*;

/// A detoasted (possibly short-header) `jsonb` varlena owned by the current
/// memory context; valid for the duration of the function call.
pub struct RawJsonb(*mut pg_sys::varlena);

impl RawJsonb {
    /// The varlena payload, i.e. the `JsonbContainer` bytes without the header.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: self.0 is a non-null, detoasted varlena that lives for the call.
        unsafe { pgrx::varlena_to_byte_slice(self.0) }
    }
}

impl FromDatum for RawJsonb {
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        _typoid: pg_sys::Oid,
    ) -> Option<Self> {
        if is_null {
            return None;
        }
        // SAFETY: a non-null jsonb datum is a (possibly toasted) varlena pointer.
        Some(RawJsonb(unsafe {
            pg_sys::pg_detoast_datum_packed(datum.cast_mut_ptr())
        }))
    }
}

// SAFETY: RawJsonb is only declared for `jsonb` arguments (see impl_sql_translatable below).
unsafe impl<'fcx> ArgAbi<'fcx> for RawJsonb {
    unsafe fn unbox_arg_unchecked(arg: Arg<'_, 'fcx>) -> Self {
        let index = arg.index();
        // SAFETY: the argument's SQL type is jsonb, which FromDatum above expects.
        unsafe { arg.unbox_arg_using_from_datum() }
            .unwrap_or_else(|| panic!("argument {index} must not be null"))
    }
}

pgrx::impl_sql_translatable!(RawJsonb, "jsonb");
