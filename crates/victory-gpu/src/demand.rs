//! Title memory the card wrote, given to the title only when it reads it
//! (`VICTORY_DEMAND_BACK=1`).
//!
//! The profile of the world (runs 11 and 12) said the frame's time goes to
//! copying back first: 75-86 MB a frame lands into title memory, while
//! what the title's own readbacks name — the only reads of card-written
//! memory the title was seen to make — are 96 bytes (two targets of 5×1
//! and 1×1). The rest is copied back because the title may read it, not
//! because it does.
//!
//! So of a merged piece, the whole 64 KiB pages are not written into title
//! memory: they are made inaccessible, and the bytes stay in the copy the
//! card wrote (which the GPU keeps using anyway). The first access of the
//! title's to such a page faults; the handler copies the page out of the
//! copy and gives the access back. The ragged edges of the piece — its
//! first and last page, when the piece does not fill them — are written at
//! once, as before, and so is any piece smaller than a page: a counter, a
//! query result, the things the title reads all the time, where a fault
//! for each would cost more than the copy.
//!
//! A page is copied as the copy holds it at the moment of the fault: the
//! card may have written it again meanwhile, and those bytes are the
//! newest truth about what the card did, which is what a read of the
//! title's is asking for.

#![cfg(windows)]


use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, PAGE_READWRITE, VirtualProtect,
    VirtualQuery,
};

use crate::shadow::Shadow;

/// The unit guarded and copied on demand. The console's page is 64 KiB,
/// but the copy-back's cost is per fault (the handler, `VirtualProtect`,
/// `VirtualQuery`: some 150 µs each), not per byte — run 20 copied
/// 39-48 MB a frame in ~600 faults for 93-96 ms. A megabyte page cuts
/// the faults sixteen-fold; the extra bytes copied are `memcpy`-cheap.
/// (`VICTORY_DEMAND_PAGE_KB` overrides, 64..=4096.)
static PAGE_KB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
pub fn page() -> usize {
    *PAGE_KB.get_or_init(|| {
        std::env::var("VICTORY_DEMAND_PAGE_KB")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&kb| (64..=4096).contains(&kb))
            .unwrap_or(64)
            << 10
    })
}

/// The pages of title memory guarded now: the page's index in the title's
/// address space, then the end of what its guard covers — the page clipped
/// to the shadow it belongs to, so a guard never reaches into a
/// neighbour's memory (a whole page past the shadow's end guarded the
/// neighbour's bytes too, and one fault gave the whole page back, leaving
/// the neighbour's unfaulted part stale: blocks of wrong texture).
static PENDING: Mutex<Option<std::collections::HashMap<usize, usize>>> = Mutex::new(None);

/// Pages guarded, and pages given on a fault, since the last frame line.
static GUARDED: AtomicU64 = AtomicU64::new(0);
static FAULTS: AtomicU64 = AtomicU64::new(0);

/// The counts for the frame-rate line: (pages guarded, pages given on a
/// fault) since the last call.
pub(crate) fn take_counts() -> (u64, u64) {
    (
        GUARDED.swap(0, Ordering::Relaxed),
        FAULTS.swap(0, Ordering::Relaxed),
    )
}

/// Whether the demand back is on (`VICTORY_DEMAND_BACK=1`).
pub(crate) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DEMAND_BACK").is_some_and(|v| v == "1"))
}

/// The card wrote `n` bytes at `at` of `shadow`'s copy, and the eager path
/// would write them into title memory now (`bytes` are what it would
/// write). Takes the whole pages of the piece ([`enabled`]): they are
/// guarded instead, and the title's first look at one copies them out of
/// the copy. The ragged edges are written at once here. False: the caller
/// writes the whole piece itself (the demand back is off, or the piece has
/// no whole page in it).
pub(crate) fn offer(shadow: &Shadow, at: usize, n: usize, bytes: &[u8]) -> bool {
    if !enabled() || n == 0 {
        return false;
    }
    install();
    let start = shadow.start as usize + at;
    let size = page();
    let first = start.div_ceil(size);
    let end = (start + n) / size; // the first page past the whole ones
    if first >= end {
        return false; // no whole page: written as before
    }
    let head = first * size - start; // the piece's bytes before the pages
    let tail = start + n - end * size; // and after them
    // SAFETY: committed title memory, plain read-write; the pages of the
    // piece are guarded only after these writes are done.
    if head > 0 {
        unsafe { crate::host::write_mapped(start as *mut u8, bytes.as_ptr(), head) };
    }
    if tail > 0 {
        unsafe {
            crate::host::write_mapped(
                (end * size) as *mut u8,
                bytes.as_ptr().add(n - tail),
                tail,
            )
        };
    }
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let pending = pending.get_or_insert_with(std::collections::HashMap::new);
    let mut guarded = 0;
    let size = page();
    // The shadow's end: a guard never reaches past it into a neighbour's
    // memory.
    let limit = shadow.start as usize + shadow.len as usize;
    for page in first..end {
        let at = page * size;
        let clipped_end = at + size;
        let clipped_end = clipped_end.min(limit);
        if let Some(_old) = pending.insert(page, clipped_end) {
            // Another piece (of another shadow, most likely) holds this
            // page. Give up the whole piece: the caller writes it eagerly,
            // and the write's touch of the other's guard is sorted out by
            // the fault handler, which gives the page back to it first.
            pending.insert(page, _old);
            guarded = 0;
            break;
        }
        if !guard_page(at, clipped_end - at) {
            pending.remove(&page); // not plain read-write memory: a guard
                                   // would turn an ordinary access into a
                                   // crash nothing of ours explains
            continue;
        }
        guarded += 1;
    }
    if guarded == 0 {
        return false;
    }
    GUARDED.fetch_add(guarded, Ordering::Relaxed);
    true
}

/// Makes one page of title memory inaccessible. False: it is not committed
/// read-write memory (a reservation, another protection), so a guard would
/// turn an ordinary access into a crash nothing of ours explains.
fn guard_page(at: usize, len: usize) -> bool {
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n =
        unsafe { VirtualQuery(at as *const _, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
    if n == 0 || info.State != MEM_COMMIT || info.Protect != PAGE_READWRITE {
        return false;
    }
    let mut old = PAGE_READWRITE;
    // SAFETY: a committed page of the title's memory, whose protection the
    // write watcher changes the same way.
    let ok = unsafe { VirtualProtect(at as *const _, len, PAGE_NOACCESS, &mut old) };
    ok != 0
}

/// The handler's work: the title touched a guarded page. Copies the page
/// out of the copy that holds it and gives the access back. False: the
/// page is none of ours.
fn hand_back(index: usize) -> bool {
    // The entry is taken only after the page's access is back: between the
    // removal and the protection's return there was a window in which a
    // second fault on the same page (another of the title's threads, or the
    // same one re-entering) found no entry and fell through as a crash
    // nothing of ours explained. Peeking keeps both handlers working on
    // the same page; the work is the same twice and safe.
    let was = {
        let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        pending.as_ref().and_then(|m| m.get(&index).copied())
    };
    let Some(end) = was else {
        // A fault on a page with no pending entry: an ordinary access
        // violation of the title's (the handler sees them all) — unless
        // the page is still NOACCESS, which would be a page our guard
        // left behind without an entry, the crash this file once made
        // possible. That case is worth a line of its own.
        #[cfg(feature = "diagnostics")]
        unsafe {
            let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
            let at = index * page();
            let n = VirtualQuery(at as *const _, &mut info, size_of::<MEMORY_BASIC_INFORMATION>());
            if n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_NOACCESS {
                tracing::warn!(index, "a fault on a page left NOACCESS with no demand entry");
            }
        }
        return false;
    };
    let size = page();
    let at = index * size;
    let len = end - at;
    // The copy the page's bytes are in, by the page's address. It may be
    // gone (the title freed the buffer while a page of it stood guarded):
    // then the guard comes off and the access runs as it is.
    if let Some(shadow) = crate::shadow::find_any(at as u64, len as u64) {
        // The guard was clipped to this shadow already; the copy takes the
        // overlap once more, in case the guard's rounding (the protection
        // works on 4 KiB pages) reached a neighbour's bytes.
        let in_shadow = at.saturating_sub(shadow.start as usize);
        let stop = (in_shadow + len).min(shadow.len as usize);
        if stop > in_shadow {
            shadow.demand_back(in_shadow, stop - in_shadow);
        }
    }
    let mut old = 0u32;
    // SAFETY: a page we made inaccessible.
    let ok = unsafe { VirtualProtect(at as *const _, len, PAGE_READWRITE, &mut old) };
    let ok = ok != 0;
    if ok {
        FAULTS.fetch_add(1, Ordering::Relaxed);
        // The entry goes only now: a fault that came in while the page was
        // still NOACCESS found the entry and did the same work again.
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = pending.as_mut() {
            m.remove(&index);
        }
    }
    ok
}

/// Registers the handler for the guarded pages, once.
pub(crate) fn install() {
    unsafe extern "system" fn on_access(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS || r.NumberParameters < 2 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // ExceptionInformation[0]: 0 a read, 1 a write. Both are taken: a
        // write of the title's to a page the card wrote lands on the
        // card's bytes, which is what its own write over them means.
        if hand_back(r.ExceptionInformation[1] / page()) {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only touches our own state and protections.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_access));
    });
}
