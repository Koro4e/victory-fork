//! A second direct queue for texture re-uploads (`VICTORY_TEXTURE_QUEUE=1`).
//!
//! The profile of the world (run 11, the card's time by command) said the
//! frame's GPU time goes to texture uploads first: about 140 ms a frame in
//! thousands of small `CopyTextureRegion`s, single ones of 165-206 ms, all
//! recorded into the frame's own lists, so they serialize with its draws.
//! The card is full, textures are let go and made again, and the churn is
//! re-uploaded every frame a list samples them.
//!
//! A re-upload shows old contents until its copy has run: the texture
//! already holds the last level of detail the title streamed. So a
//! re-upload may run on another queue, at a present (the main queue is
//! settling then, and the next frame is only being recorded), while the
//! frame that asks for it renders without the copies in its lists. The
//! main queue waits for the copies' fence before its next submission: by
//! then the copies have usually run during the recording of the frame,
//! which at these frame times is 100-200 ms of the queue standing idle.
//!
//! The copies are recorded with no barriers: the state the texture is in
//! when the copy runs is whatever the title last left it in (a shader
//! resource, most often). A copy out of that state is outside what
//! D3D12 promises; on the cards that have run it, a copy into a texture
//! does what it says. The first upload of a texture keeps the old inline
//! path: a texture never uploaded has nothing to show, and the frame
//! would sample black.

#![cfg(windows)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;

use crate::texture::Staging;

/// Whether the second queue is on, and how:
/// `VICTORY_TEXTURE_QUEUE=1` — a second direct queue, the main queue
/// waits for its fence before the next submission;
/// `VICTORY_TEXTURE_QUEUE=2` — a copy queue (the card's copy engine, which
/// runs beside its 3D), and no wait at all: a texture whose copy has not
/// landed yet shows the contents it had, which for a re-upload is what it
/// showed anyway.
pub(crate) fn mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_QUEUE")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    })
}

pub(crate) fn enabled() -> bool {
    mode() != 0
}

/// Whether the main queue waits for the copies' fence (mode 1 only).
pub(crate) fn waits() -> bool {
    mode() == 1
}

struct CopyQ {
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    /// Re-uploads waiting for the next flush: their staging (which holds
    /// the bytes and keeps them alive) and the host texture.
    pending: Vec<(std::sync::Arc<Staging>, windows::Win32::Graphics::Direct3D12::ID3D12Resource)>,
    /// The last value signalled on the fence.
    value: AtomicU64,
    /// The last value the main queue was told to wait for.
    waited: AtomicU64,
}

static COPYQ: Mutex<Option<CopyQ>> = Mutex::new(None);

/// Re-uploads run by the second queue since the last frame line.
static FLUSHED: AtomicU64 = AtomicU64::new(0);

/// The re-uploads the second queue ran since the last call.
pub(crate) fn take_flushed() -> u64 {
    FLUSHED.swap(0, Ordering::Relaxed)
}

/// The copy queue, made once from the host device.
fn made() -> bool {
    let mut q = COPYQ.lock().unwrap_or_else(|e| e.into_inner());
    if q.is_some() {
        return true;
    }
    let Some(host) = crate::host::host() else {
        return false;
    };
    // SAFETY: plain object creation on the live device.
    let made: windows::core::Result<(ID3D12CommandQueue, ID3D12Fence, ID3D12CommandAllocator, ID3D12GraphicsCommandList)> =
        unsafe {
            (|| {
                // Mode 2 asks for the card's copy engine: its work runs
                // beside the 3D of the main queue, not instead of it.
                let kind = if mode() == 2 {
                    D3D12_COMMAND_LIST_TYPE_COPY
                } else {
                    D3D12_COMMAND_LIST_TYPE_DIRECT
                };
                let queue = host.device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: kind,
                    ..Default::default()
                })?;
                let fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
                let allocator = host.device.CreateCommandAllocator(kind)?;
                let list: ID3D12GraphicsCommandList = host.device
                    .CreateCommandList(0, kind, &allocator, None)?;
                list.Close()?;
                Ok((queue, fence, allocator, list))
            })()
        };
    match made {
        Ok((queue, fence, allocator, list)) => {
            *q = Some(CopyQ {
                queue,
                fence,
                allocator,
                list,
                pending: Vec::new(),
                value: AtomicU64::new(0),
                waited: AtomicU64::new(0),
            });
            tracing::info!(
                mode = mode(),
                "a second queue for texture re-uploads (VICTORY_TEXTURE_QUEUE)"
            );
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "no second queue for texture re-uploads");
            false
        }
    }
}

/// A re-upload for the second queue: the copy runs at the next present.
/// The staging buffer is held until then, so its bytes stay.
pub(crate) fn schedule(staging: &std::sync::Arc<Staging>, texture: &windows::Win32::Graphics::Direct3D12::ID3D12Resource) {
    if !made() {
        return;
    }
    let mut q = COPYQ.lock().unwrap_or_else(|e| e.into_inner());
    let Some(q) = q.as_mut() else { return };
    q.pending.push((staging.clone(), texture.clone()));
}

/// Records and runs what is pending, at a present: the main queue has
/// settled, and the next frame is only being recorded. If the allocator's
/// last work has not run yet, what is pending waits for the next present.
pub(crate) fn flush() {
    if !enabled() {
        return;
    }
    let mut q = COPYQ.lock().unwrap_or_else(|e| e.into_inner());
    let Some(q) = q.as_mut() else { return };
    if q.pending.is_empty() {
        return;
    }
    let last = q.value.load(Ordering::Relaxed);
    // SAFETY: a live fence.
    if last != 0 && unsafe { q.fence.GetCompletedValue() } < last {
        return; // the last batch still runs; these wait for the next present
    }
    let scheduled = q.pending.len();
    // SAFETY: the allocator's last list has run (the fence above).
    unsafe {
        let _ = q.allocator.Reset();
        let _ = q.list.Reset(&q.allocator, None);
    }
    for (staging, texture) in &q.pending {
        // SAFETY: live resources; the footprints name the staging buffer.
        for (i, footprint) in staging.footprints.iter().enumerate() {
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: i as u32,
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(&staging.buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: *footprint,
                },
            };
            // SAFETY: an open list; both locations name live resources.
            unsafe { q.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }
    // SAFETY: a closed-able open list.
    unsafe {
        let _ = q.list.Close();
        let value = last + 1;
        q.queue
            .ExecuteCommandLists(&[Some(q.list.cast::<ID3D12CommandList>().unwrap())]);
        let _ = q.queue.Signal(&q.fence, value);
        q.value.store(value, Ordering::Relaxed);
    }
    FLUSHED.fetch_add(scheduled as u64, Ordering::Relaxed);
    q.pending.clear();
}

/// The main queue waits for what the second queue has run, before its
/// next submission (mode 1 only: mode 2 lets a texture show what it had
/// until its copy lands). Called where a submission is about to run.
pub(crate) fn wait_due(queue: &ID3D12CommandQueue) {
    if !waits() {
        return;
    }
    let q = COPYQ.lock().unwrap_or_else(|e| e.into_inner());
    let Some(q) = q.as_ref() else { return };
    let value = q.value.load(Ordering::Relaxed);
    let waited = q.waited.load(Ordering::Relaxed);
    if value > waited {
        // SAFETY: a live queue and fence.
        unsafe {
            let _ = queue.Wait(&q.fence, value);
        }
        q.waited.store(value, Ordering::Relaxed);
    }
}
