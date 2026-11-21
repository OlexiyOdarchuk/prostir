//! Counts allocations in development builds (a store build has none of this): all
//! threads together, and the main thread's alone, so the main loop can say how many
//! a frame makes (in the log every 10 s, and the selftest's `alloc_per_frame`).

#[cfg(not(feature = "store"))]
mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    static ALL: AtomicU64 = AtomicU64::new(0);
    static MAIN: AtomicU64 = AtomicU64::new(0);
    /// The main thread's `pthread_t`; 0 before it is known.
    static MAIN_THREAD: AtomicUsize = AtomicUsize::new(0);

    /// The system's allocator, counting.
    struct Counting;

    fn me() -> usize {
        // SAFETY: pthread_self only reads the calling thread's own handle.
        unsafe { libc::pthread_self() as usize }
    }

    fn note() {
        ALL.fetch_add(1, Ordering::Relaxed);
        if me() == MAIN_THREAD.load(Ordering::Relaxed) {
            MAIN.fetch_add(1, Ordering::Relaxed);
        }
    }

    // SAFETY: every call goes to the system allocator unchanged; counting allocates
    // nothing and takes no lock.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note();
            // SAFETY: the caller's promises about `layout` pass on as they are.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note();
            // SAFETY: as in `alloc`.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note();
            // SAFETY: `ptr` came from this allocator (the system's) with `layout`, as
            // the caller promises.
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: as in `realloc`.
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static COUNTING: Counting = Counting;

    pub fn set_main_thread() {
        MAIN_THREAD.store(me(), Ordering::Relaxed);
    }

    pub fn main_thread() -> Option<u64> {
        Some(MAIN.load(Ordering::Relaxed))
    }

    pub fn all() -> Option<u64> {
        Some(ALL.load(Ordering::Relaxed))
    }
}

#[cfg(feature = "store")]
mod counting {
    pub fn set_main_thread() {}

    pub fn main_thread() -> Option<u64> {
        None
    }

    pub fn all() -> Option<u64> {
        None
    }
}

/// `set_main_thread`: the calling thread is the main one from now on (each start of
/// the app). `main_thread`, `all`: allocations so far, the main thread's and every
/// thread's (`None` in a store build).
pub use counting::{all, main_thread, set_main_thread};

/// How often the main loop logs its allocations.
const LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// The main loop's allocations, logged every [`LOG_EVERY`]: how many a frame on the
/// main thread, how many a second on all threads.
#[derive(Default)]
pub struct Tally {
    /// When the count began, the frames since, and both counts then.
    since: Option<(std::time::Instant, u64, u64, u64)>,
}

impl Tally {
    /// A frame was drawn.
    pub fn frame(&mut self) {
        let (Some(main), Some(all)) = (main_thread(), all()) else {
            return;
        };
        let now = std::time::Instant::now();
        let (since, frames, main_then, all_then) = self.since.get_or_insert((now, 0, main, all));
        *frames += 1;
        let window = now - *since;
        if window < LOG_EVERY {
            return;
        }
        log::info!(
            "allocations: {:.1} a frame on the main thread over {frames} frames, {:.0} a second on all threads",
            (main - *main_then) as f64 / *frames as f64,
            (all - *all_then) as f64 / window.as_secs_f64()
        );
        self.since = Some((now, 0, main, all));
    }
}
