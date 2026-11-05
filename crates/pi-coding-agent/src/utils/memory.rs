//! Process memory tuning (no TypeScript counterpart: V8 manages its own heap).
//!
//! glibc's malloc gives every thread that allocates concurrently its own arena (up to eight per
//! CPU), and memory freed inside an arena stays resident until it is trimmed. pi's threads (tokio
//! workers, the TUI loop, the JavaScript runtime) allocate briefly and then sit idle, so most of
//! that memory is fragmentation: limiting the arenas and trimming at idle points keeps the
//! resident size close to the live heap. Neither changes behavior.

/// Limit glibc's malloc arenas. Called first thing in `main`, before other threads exist. A limit
/// configured by the user (`MALLOC_ARENA_MAX`, `GLIBC_TUNABLES=glibc.malloc.arena_max=...`) wins.
pub fn configure_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let configured = std::env::var_os("MALLOC_ARENA_MAX").is_some()
            || std::env::var("GLIBC_TUNABLES").is_ok_and(|tunables| tunables.contains("arena_max"));
        if !configured {
            // SAFETY: mallopt only adjusts allocator parameters.
            unsafe {
                libc::mallopt(libc::M_ARENA_MAX, 2);
            }
        }
    }
}

/// Return free heap memory to the operating system (`malloc_trim`). Cheap enough for idle points
/// (startup finished, an agent run settled); blocks for a few milliseconds on a large heap, so
/// async callers run it on a blocking thread.
pub fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases free pages of the allocator's own arenas.
    unsafe {
        libc::malloc_trim(0);
    };
}

/// [`release_free_memory`] on tokio's blocking pool, when called inside a runtime.
pub fn release_free_memory_in_background() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        drop(handle.spawn_blocking(release_free_memory));
    };
}
