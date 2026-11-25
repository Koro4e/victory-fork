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

use std::collections::HashSet;
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

/// The unit guarded and copied on demand: 64 KiB, the console's page (the
/// same unit [`crate::watch`] tracks writes by).
const PAGE: usize = 64 << 10;

/// The pages of title memory guarded now (their number in the title's
/// address space).
static PENDING: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

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
    let first = start.div_ceil(PAGE);
    let end = (start + n) / PAGE; // the first page past the whole ones
    if first >= end {
        return false; // no whole page: written as before
    }
    let head = first * PAGE - start; // the piece's bytes before the pages
    let tail = start + n - end * PAGE; // and after them
    // SAFETY: committed title memory, plain read-write; the pages of the
    // piece are guarded only after these writes are done.
    if head > 0 {
        unsafe { crate::host::write_mapped(start as *mut u8, bytes.as_ptr(), head) };
    }
    if tail > 0 {
        unsafe {
            crate::host::write_mapped(
                (end * PAGE) as *mut u8,
                bytes.as_ptr().add(n - tail),
                tail,
            )
        };
    }
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let pending = pending.get_or_insert_with(HashSet::new);
    let mut guarded = 0;
    for page in first..end {
        if !pending.insert(page) {
            continue; // already guarded: the copy holds the newest bytes
        }
        if !guard_page(page * PAGE) {
            pending.remove(&page); // not plain read-write memory: a guard
                                   // would turn an ordinary access into a
                                   // crash nothing of ours explains
            continue;
        }
        guarded += 1;
    }
    GUARDED.fetch_add(guarded, Ordering::Relaxed);
    true
}

/// Makes one page of title memory inaccessible. False: it is not committed
/// read-write memory (a reservation, another protection), so a guard would
/// turn an ordinary access into a crash nothing of ours explains.
fn guard_page(at: usize) -> bool {
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
    let ok = unsafe { VirtualProtect(at as *const _, PAGE, PAGE_NOACCESS, &mut old) };
    ok != 0
}

/// The handler's work: the title touched a guarded page. Copies the page
/// out of the copy that holds it and gives the access back. False: the
/// page is none of ours.
fn hand_back(page: usize) -> bool {
    let was = {
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        pending
            .as_mut()
            .map(|m| m.remove(&page))
            .unwrap_or(false)
    };
    if !was {
        return false;
    }
    let at = page * PAGE;
    // The copy the page's bytes are in, by the page's address. It may be
    // gone (the title freed the buffer while a page of it stood guarded):
    // then the guard comes off and the access runs as it is.
    if let Some(shadow) = crate::shadow::find_any(at as u64, PAGE as u64) {
        let in_shadow = at - shadow.start as usize;
        if in_shadow + PAGE <= shadow.len as usize {
            shadow.demand_back(in_shadow, PAGE);
        }
    }
    let mut old = 0u32;
    // SAFETY: a page we made inaccessible.
    let ok = unsafe { VirtualProtect(at as *const _, PAGE, PAGE_READWRITE, &mut old) };
    let ok = ok != 0;
    if ok {
        FAULTS.fetch_add(1, Ordering::Relaxed);
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
        if hand_back(r.ExceptionInformation[1] / PAGE) {
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
