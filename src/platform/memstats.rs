//! Process memory figures for the periodic health line in the log (field report 2026-10-11:
//! "1.1 GB after 6.5 h" could not be attributed from the log). Linux only; every figure is
//! optional and the line simply leaves out what can't be read.

/// One sample. Sizes in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemStats {
    /// Resident set size (`VmRSS`).
    pub rss: Option<u64>,
    /// Anonymous resident memory (`RssAnon`): the heaps, mpv's decoder frames.
    pub rss_anon: Option<u64>,
    /// Swapped out (`VmSwap`).
    pub swap: Option<u64>,
    /// Peak resident set size (`VmHWM`).
    pub hwm: Option<u64>,
    /// glibc heap bytes in use (`mallinfo2().uordblks`, all arenas).
    pub heap_in_use: Option<u64>,
    /// glibc heap bytes free but still held by the allocator (`fordblks`): what
    /// `malloc_trim` could give back.
    pub heap_free: Option<u64>,
}

impl MemStats {
    /// Read the current process's figures.
    pub fn sample() -> Self {
        let mut s = std::fs::read_to_string("/proc/self/status").map(|t| parse_status(&t)).unwrap_or_default();
        (s.heap_in_use, s.heap_free) = heap();
        s
    }

    /// `rss 690 MiB (anon 610) · swap 470 MiB · peak 1070 MiB · heap 349 MiB in use, 335 MiB free`.
    pub fn line(&self) -> String {
        let mib = |b: u64| b / (1024 * 1024);
        let mut parts = Vec::new();
        if let Some(r) = self.rss {
            parts.push(match self.rss_anon {
                Some(a) => format!("rss {} MiB (anon {})", mib(r), mib(a)),
                None => format!("rss {} MiB", mib(r)),
            });
        }
        if let Some(s) = self.swap {
            parts.push(format!("swap {} MiB", mib(s)));
        }
        if let Some(h) = self.hwm {
            parts.push(format!("peak {} MiB", mib(h)));
        }
        if let (Some(u), Some(f)) = (self.heap_in_use, self.heap_free) {
            parts.push(format!("heap {} MiB in use, {} MiB free", mib(u), mib(f)));
        }
        if parts.is_empty() { "memory figures unavailable".into() } else { parts.join(" · ") }
    }
}

/// The `kB` fields of `/proc/<pid>/status` we report.
fn parse_status(text: &str) -> MemStats {
    let mut s = MemStats::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let bytes = || value.trim().strip_suffix("kB").and_then(|n| n.trim().parse::<u64>().ok()).map(|kb| kb * 1024);
        match key {
            "VmRSS" => s.rss = bytes(),
            "RssAnon" => s.rss_anon = bytes(),
            "VmSwap" => s.swap = bytes(),
            "VmHWM" => s.hwm = bytes(),
            _ => {}
        }
    }
    s
}

/// glibc's heap statistics (in use, free); `(None, None)` elsewhere.
fn heap() -> (Option<u64>, Option<u64>) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        /// `struct mallinfo2` (glibc ≥ 2.33): ten `size_t` fields.
        #[repr(C)]
        #[derive(Default)]
        struct MallInfo2 {
            arena: usize,
            ordblks: usize,
            smblks: usize,
            hblks: usize,
            hblkhd: usize,
            usmblks: usize,
            fsmblks: usize,
            uordblks: usize,
            fordblks: usize,
            keepcost: usize,
        }
        unsafe extern "C" {
            fn mallinfo2() -> MallInfo2;
        }
        // SAFETY: plain glibc call without arguments; it returns a struct by value (it locks
        // each arena briefly, so it is called every few minutes, never per frame).
        let m = unsafe { mallinfo2() };
        // mmapped chunks (hblkhd, large allocations) are in use too
        (Some((m.uordblks + m.hblkhd) as u64), Some(m.fordblks as u64))
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_fields_are_parsed_in_bytes() {
        let s = parse_status("Name:\ttuner\nVmHWM:\t 1070000 kB\nVmRSS:\t  706560 kB\nRssAnon:\t  624640 kB\nVmSwap:\t  481280 kB\n");
        assert_eq!(s.rss, Some(706_560 * 1024));
        assert_eq!(s.rss_anon, Some(624_640 * 1024));
        assert_eq!(s.swap, Some(481_280 * 1024));
        assert_eq!(s.hwm, Some(1_070_000 * 1024));
        assert_eq!(
            MemStats { heap_in_use: Some(349 << 20), heap_free: Some(335 << 20), ..s }.line(),
            "rss 690 MiB (anon 610) · swap 470 MiB · peak 1044 MiB · heap 349 MiB in use, 335 MiB free"
        );
        assert_eq!(MemStats::default().line(), "memory figures unavailable");
    }

    /// The real process: an RSS, and on glibc a heap that holds what this test allocated.
    #[test]
    fn this_process_reports_its_memory() {
        let block = vec![1u8; 8 << 20];
        let s = MemStats::sample();
        assert!(s.rss.is_some_and(|r| r > 0), "{s:?}");
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        assert!(s.heap_in_use.is_some_and(|u| u >= 8 << 20), "{s:?}");
        drop(std::hint::black_box(block));
    }
}
