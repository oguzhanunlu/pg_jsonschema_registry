//! Nesting-depth guard for `jsonb` documents and schemas.
//!
//! The validator recurses once (or more) per nesting level of the document, with no
//! depth limit of its own, and a stack overflow in a backend is a SIGSEGV that
//! restarts every session. So documents nested deeper than [`MAX_DEPTH`] are refused
//! before validation. The walk over the on-disk `JsonbContainer` is iterative (an
//! explicit stack of frames), so it cannot overflow itself.
//!
//! Format (`src/include/utils/jsonb.h`): a container is a `uint32` header (low 28 bits
//! the count, plus `JB_FOBJECT` / `JB_FARRAY` / `JB_FSCALAR`), then one `JEntry` per
//! child (two per pair for objects: all keys, then all values), then the children's
//! data. A `JEntry`'s low 28 bits are the child's length, or, when `JENTRY_HAS_OFF` is
//! set, its end offset relative to the start of the data area; bits 28-30 are the
//! type. A child container starts at its offset rounded up to 4 (`INTALIGN`).

/// Deepest document nesting `valid`, `errors` and `validate` accept.
pub const MAX_DEPTH: usize = 4096;

/// Deepest schema nesting `register` accepts. Lower than [`MAX_DEPTH`]: stored schemas
/// are read through `serde_json`, whose parser refuses more than 127 levels.
pub const SCHEMA_MAX_DEPTH: usize = 100;

const JB_CMASK: u32 = 0x0FFF_FFFF;
const JB_FOBJECT: u32 = 0x2000_0000;
const JENTRY_OFFLENMASK: u32 = 0x0FFF_FFFF;
const JENTRY_TYPEMASK: u32 = 0x7000_0000;
const JENTRY_HAS_OFF: u32 = 0x8000_0000;
const JENTRY_ISCONTAINER: u32 = 0x5000_0000;

/// Smallest number of bytes one extra nesting level adds: the child's `JEntry` in its
/// parent plus the child container's header.
const MIN_BYTES_PER_LEVEL: usize = 8;

#[cfg(any(test, feature = "pg_test"))]
thread_local! {
    /// Number of full walks done (test observability for the size gate).
    pub static WALKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn read_u32(c: &[u8], pos: usize) -> Option<u32> {
    let bytes = c.get(pos..pos.checked_add(4)?)?;
    Some(u32::from_ne_bytes(bytes.try_into().ok()?))
}

/// Whether the `JsonbContainer` in `container` (a `jsonb` varlena's payload) nests
/// deeper than `limit` levels (the root container is level 1). A container shorter
/// than `MIN_BYTES_PER_LEVEL * limit` bytes cannot, and is not walked.
pub fn exceeds_depth(container: &[u8], limit: usize) -> bool {
    if container.len() < MIN_BYTES_PER_LEVEL.saturating_mul(limit) {
        return false;
    }
    walk_exceeds(container, limit)
}

/// One container being scanned: where its `JEntry` array and data area start, how many
/// entries it has, the next entry to look at, and that entry's start offset.
struct Frame {
    entries: usize,
    data: usize,
    count: usize,
    next: usize,
    offset: usize,
}

fn frame_at(c: &[u8], pos: usize) -> Option<Frame> {
    let header = read_u32(c, pos)?;
    let mut count = (header & JB_CMASK) as usize;
    if header & JB_FOBJECT != 0 {
        count = count.checked_mul(2)?;
    }
    let entries = pos.checked_add(4)?;
    let data = entries.checked_add(count.checked_mul(4)?)?;
    (data <= c.len()).then_some(Frame {
        entries,
        data,
        count,
        next: 0,
        offset: 0,
    })
}

/// The walk itself, without the size gate. Malformed input stops the walk (`false`).
fn walk_exceeds(c: &[u8], limit: usize) -> bool {
    #[cfg(any(test, feature = "pg_test"))]
    WALKS.with(|w| w.set(w.get() + 1));
    let Some(root) = frame_at(c, 0) else {
        return false;
    };
    let mut stack = vec![root];
    if stack.len() > limit {
        return true;
    }
    while let Some(top) = stack.last_mut() {
        if top.next == top.count {
            stack.pop();
            continue;
        }
        let Some(entry) = read_u32(c, top.entries + 4 * top.next) else {
            return false;
        };
        let start = top.offset;
        let field = (entry & JENTRY_OFFLENMASK) as usize;
        top.offset = if entry & JENTRY_HAS_OFF != 0 {
            field
        } else {
            start + field
        };
        top.next += 1;
        if entry & JENTRY_TYPEMASK != JENTRY_ISCONTAINER {
            continue;
        }
        let child = top.data + start.next_multiple_of(4);
        let Some(frame) = frame_at(c, child) else {
            return false;
        };
        stack.push(frame);
        if stack.len() > limit {
            return true;
        }
    }
    false
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use crate::depth::{exceeds_depth, walk_exceeds, WALKS};
    use pgrx::prelude::*;

    /// The container bytes of `sql`'s jsonb result.
    fn bytes(sql: &str) -> Vec<u8> {
        Spi::connect(|client| {
            let row = client
                .select(&format!("SELECT ({sql})::jsonb"), Some(1), &[])?
                .first()
                .get_datum_by_ordinal(1)?
                .unwrap();
            // SAFETY: the datum is a jsonb varlena that lives for this SPI connection.
            let raw = unsafe {
                <crate::raw::RawJsonb as FromDatum>::from_polymorphic_datum(
                    row,
                    false,
                    pg_sys::JSONBOID,
                )
            }
            .unwrap();
            Ok::<_, pgrx::spi::Error>(raw.as_bytes().to_vec())
        })
        .unwrap()
    }

    #[pg_test]
    fn walk_measures_nesting() {
        let d3 = bytes("'[1, {\"a\": [\"x\", 2]}, \"y\"]'");
        assert!(!walk_exceeds(&d3, 3));
        assert!(walk_exceeds(&d3, 2));
        let scalar = bytes("'5'");
        assert!(!walk_exceeds(&scalar, 1));
        // wide containers: more than 32 entries, so some JEntrys store offsets
        let wide = bytes(
            "(SELECT jsonb_agg(CASE WHEN g = 40 THEN '[[[1]]]'::jsonb ELSE to_jsonb(repeat('s', g)) END) FROM generate_series(1, 50) g)",
        );
        assert!(!walk_exceeds(&wide, 4));
        assert!(walk_exceeds(&wide, 3));
        let obj = bytes(
            "(SELECT jsonb_object_agg('k' || g, CASE WHEN g = 37 THEN '{\"z\": [1]}'::jsonb ELSE to_jsonb(g) END) FROM generate_series(1, 40) g)",
        );
        assert!(!walk_exceeds(&obj, 3));
        assert!(walk_exceeds(&obj, 2));
        let deep = bytes("repeat('[', 300) || repeat(']', 300)");
        assert!(!walk_exceeds(&deep, 300));
        assert!(walk_exceeds(&deep, 299));
    }

    #[pg_test]
    fn small_containers_skip_the_walk() {
        let d3 = bytes("'[[[1]]]'");
        let before = WALKS.with(|w| w.get());
        assert!(!exceeds_depth(&d3, 4096));
        assert_eq!(
            WALKS.with(|w| w.get()),
            before,
            "a small document was walked"
        );
        let deep = bytes("repeat('[', 5000) || repeat(']', 5000)");
        assert!(deep.len() >= 8 * 4096);
        assert!(exceeds_depth(&deep, 4096));
        assert_eq!(WALKS.with(|w| w.get()), before + 1);
    }
}
