//! # Rust Compiler Self-Profiling
//!
//! This module implements the basic framework for the compiler's self-
//! profiling support. It provides the `SelfProfiler` type which enables
//! recording "events". An event is something that starts and ends at a given
//! point in time and has an ID and a kind attached to it. This allows for
//! tracing the compiler's activity.
//!
//! Internally this module uses the custom tailored [measureme][mm] crate for
//! efficiently recording events to disk in a compact format that can be
//! post-processed and analyzed by the suite of tools in the `measureme`
//! project. The highest priority for the tracing framework is on incurring as
//! little overhead as possible.
//!
//!
//! ## Event Overview
//!
//! Events have a few properties:
//!
//! - The `event_kind` designates the broad category of an event (e.g. does it
//!   correspond to the execution of a query provider or to loading something
//!   from the incr. comp. on-disk cache, etc).
//! - The `event_id` designates the query invocation or function call it
//!   corresponds to, possibly including the query key or function arguments.
//! - Each event stores the ID of the thread it was recorded on.
//! - The timestamp stores beginning and end of the event, or the single point
//!   in time it occurred at for "instant" events.
//!
//!
//! ## Event Filtering
//!
//! Event generation can be filtered by event kind. Recording all possible
//! events generates a lot of data, much of which is not needed for most kinds
//! of analysis. So, in order to keep overhead as low as possible for a given
//! use case, the `SelfProfiler` will only record the kinds of events that
//! pass the filter specified as a command line argument to the compiler.
//!
//!
//! ## `event_id` Assignment
//!
//! As far as `measureme` is concerned, `event_id`s are just strings. However,
//! it would incur too much overhead to generate and persist each `event_id`
//! string at the point where the event is recorded. In order to make this more
//! efficient `measureme` has two features:
//!
//! - Strings can share their content, so that re-occurring parts don't have to
//!   be copied over and over again. One allocates a string in `measureme` and
//!   gets back a `StringId`. This `StringId` is then used to refer to that
//!   string. `measureme` strings are actually DAGs of string components so that
//!   arbitrary sharing of substrings can be done efficiently. This is useful
//!   because `event_id`s contain lots of redundant text like query names or
//!   def-path components.
//!
//! - `StringId`s can be "virtual" which means that the client picks a numeric
//!   ID according to some application-specific scheme and can later make that
//!   ID be mapped to an actual string. This is used to cheaply generate
//!   `event_id`s while the events actually occur, causing little timing
//!   distortion, and then later map those `StringId`s, in bulk, to actual
//!   `event_id` strings. This way the largest part of the tracing overhead is
//!   localized to one contiguous chunk of time.
//!
//! How are these `event_id`s generated in the compiler? For things that occur
//! infrequently (e.g. "generic activities"), we just allocate the string the
//! first time it is used and then keep the `StringId` in a hash table. This
//! is implemented in `SelfProfiler::get_or_alloc_cached_string()`.
//!
//! For queries it gets more interesting: First we need a unique numeric ID for
//! each query invocation (the `QueryInvocationId`). This ID is used as the
//! virtual `StringId` we use as `event_id` for a given event. This ID has to
//! be available both when the query is executed and later, together with the
//! query key, when we allocate the actual `event_id` strings in bulk.
//!
//! We could make the compiler generate and keep track of such an ID for each
//! query invocation but luckily we already have something that fits all the
//! the requirements: the query's `DepNodeIndex`. So we use the numeric value
//! of the `DepNodeIndex` as `event_id` when recording the event and then,
//! just before the query context is dropped, we walk the entire query cache
//! (which stores the `DepNodeIndex` along with the query key for each
//! invocation) and allocate the corresponding strings together with a mapping
//! for `DepNodeIndex as StringId`.
//!
//! [mm]: https://github.com/rust-lang/measureme/

use crate::fx::FxHashMap;

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::thread::ThreadId;
use std::time::{Duration, Instant};
use std::u32;

use measureme::{EventId, EventIdBuilder, SerializableString, StringId};
use parking_lot::RwLock;

/// MmapSerializatioSink is faster on macOS and Linux
/// but FileSerializationSink is faster on Windows
#[cfg(not(windows))]
type SerializationSink = measureme::MmapSerializationSink;
#[cfg(windows)]
type SerializationSink = measureme::FileSerializationSink;

type Profiler = measureme::Profiler<SerializationSink>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Ord, PartialOrd)]
pub enum ProfileCategory {
    Parsing,
    Expansion,
    TypeChecking,
    BorrowChecking,
    Codegen,
    Linking,
    Other,
}

bitflags::bitflags! {
    struct EventFilter: u32 {
        const GENERIC_ACTIVITIES = 1 << 0;
        const QUERY_PROVIDERS    = 1 << 1;
        const QUERY_CACHE_HITS   = 1 << 2;
        const QUERY_BLOCKED      = 1 << 3;
        const INCR_CACHE_LOADS   = 1 << 4;

        const QUERY_KEYS         = 1 << 5;

        const DEFAULT = Self::GENERIC_ACTIVITIES.bits |
                        Self::QUERY_PROVIDERS.bits |
                        Self::QUERY_BLOCKED.bits |
                        Self::INCR_CACHE_LOADS.bits;

        // empty() and none() aren't const-fns unfortunately
        const NONE = 0;
        const ALL  = !Self::NONE.bits;
    }
}

const EVENT_FILTERS_BY_NAME: &[(&str, EventFilter)] = &[
    ("none", EventFilter::NONE),
    ("all", EventFilter::ALL),
    ("generic-activity", EventFilter::GENERIC_ACTIVITIES),
    ("query-provider", EventFilter::QUERY_PROVIDERS),
    ("query-cache-hit", EventFilter::QUERY_CACHE_HITS),
    ("query-blocked", EventFilter::QUERY_BLOCKED),
    ("incr-cache-load", EventFilter::INCR_CACHE_LOADS),
    ("query-keys", EventFilter::QUERY_KEYS),
];

fn thread_id_to_u32(tid: ThreadId) -> u32 {
    unsafe { std::mem::transmute::<ThreadId, u64>(tid) as u32 }
}

/// Something that uniquely identifies a query invocation.
pub struct QueryInvocationId(pub u32);

/// A reference to the SelfProfiler. It can be cloned and sent across thread
/// boundaries at will.
#[derive(Clone)]
pub struct SelfProfilerRef {
    // This field is `None` if self-profiling is disabled for the current
    // compilation session.
    profiler: Option<Arc<SelfProfiler>>,

    // We store the filter mask directly in the reference because that doesn't
    // cost anything and allows for filtering with checking if the profiler is
    // actually enabled.
    event_filter_mask: EventFilter,

    // Print verbose generic activities to stdout
    print_verbose_generic_activities: bool,

    // Print extra verbose generic activities to stdout
    print_extra_verbose_generic_activities: bool,
}

impl SelfProfilerRef {
    pub fn new(
        profiler: Option<Arc<SelfProfiler>>,
        print_verbose_generic_activities: bool,
        print_extra_verbose_generic_activities: bool,
    ) -> SelfProfilerRef {
        // If there is no SelfProfiler then the filter mask is set to NONE,
        // ensuring that nothing ever tries to actually access it.
        let event_filter_mask =
            profiler.as_ref().map(|p| p.event_filter_mask).unwrap_or(EventFilter::NONE);

        SelfProfilerRef {
            profiler,
            event_filter_mask,
            print_verbose_generic_activities,
            print_extra_verbose_generic_activities,
        }
    }

    // This shim makes sure that calls only get executed if the filter mask
    // lets them pass. It also contains some trickery to make sure that
    // code is optimized for non-profiling compilation sessions, i.e. anything
    // past the filter check is never inlined so it doesn't clutter the fast
    // path.
    #[inline(always)]
    fn exec<F>(&self, event_filter: EventFilter, f: F) -> TimingGuard<'_>
    where
        F: for<'a> FnOnce(&'a SelfProfiler) -> TimingGuard<'a>,
    {
        #[inline(never)]
        fn cold_call<F>(profiler_ref: &SelfProfilerRef, f: F) -> TimingGuard<'_>
        where
            F: for<'a> FnOnce(&'a SelfProfiler) -> TimingGuard<'a>,
        {
            let profiler = profiler_ref.profiler.as_ref().unwrap();
            f(&**profiler)
        }

        if unlikely!(self.event_filter_mask.contains(event_filter)) {
            cold_call(self, f)
        } else {
            TimingGuard::none()
        }
    }

    /// Start profiling a verbose generic activity. Profiling continues until the
    /// VerboseTimingGuard returned from this call is dropped. In addition to recording
    /// a measureme event, "verbose" generic activities also print a timing entry to
    /// stdout if the compiler is invoked with -Ztime or -Ztime-passes.
    #[inline(always)]
    pub fn verbose_generic_activity<'a>(
        &'a self,
        event_id: &'static str,
    ) -> VerboseTimingGuard<'a> {
        VerboseTimingGuard::start(
            event_id,
            self.print_verbose_generic_activities,
            self.generic_activity(event_id),
        )
    }

    /// Start profiling a extra verbose generic activity. Profiling continues until the
    /// VerboseTimingGuard returned from this call is dropped. In addition to recording
    /// a measureme event, "extra verbose" generic activities also print a timing entry to
    /// stdout if the compiler is invoked with -Ztime-passes.
    #[inline(always)]
    pub fn extra_verbose_generic_activity<'a>(
        &'a self,
        event_id: &'a str,
    ) -> VerboseTimingGuard<'a> {
        // FIXME: This does not yet emit a measureme event
        // because callers encode arguments into `event_id`.
        VerboseTimingGuard::start(
            event_id,
            self.print_extra_verbose_generic_activities,
            TimingGuard::none(),
        )
    }

    /// Start profiling a generic activity. Profiling continues until the
    /// TimingGuard returned from this call is dropped.
    #[inline(always)]
    pub fn generic_activity(&self, event_id: &'static str) -> TimingGuard<'_> {
        self.exec(EventFilter::GENERIC_ACTIVITIES, |profiler| {
            let event_id = profiler.get_or_alloc_cached_string(event_id);
            let event_id = EventId::from_label(event_id);
            TimingGuard::start(profiler, profiler.generic_activity_event_kind, event_id)
        })
    }

    /// Start profiling a query provider. Profiling continues until the
    /// TimingGuard returned from this call is dropped.
    #[inline(always)]
    pub fn query_provider(&self) -> TimingGuard<'_> {
        self.exec(EventFilter::QUERY_PROVIDERS, |profiler| {
            TimingGuard::start(profiler, profiler.query_event_kind, EventId::INVALID)
        })
    }

    /// Record a query in-memory cache hit.
    #[inline(always)]
    pub fn query_cache_hit(&self, query_invocation_id: QueryInvocationId) {
        self.instant_query_event(
            |profiler| profiler.query_cache_hit_event_kind,
            query_invocation_id,
            EventFilter::QUERY_CACHE_HITS,
        );
    }

    /// Start profiling a query being blocked on a concurrent execution.
    /// Profiling continues until the TimingGuard returned from this call is
    /// dropped.
    #[inline(always)]
    pub fn query_blocked(&self) -> TimingGuard<'_> {
        self.exec(EventFilter::QUERY_BLOCKED, |profiler| {
            TimingGuard::start(profiler, profiler.query_blocked_event_kind, EventId::INVALID)
        })
    }

    /// Start profiling how long it takes to load a query result from the
    /// incremental compilation on-disk cache. Profiling continues until the
    /// TimingGuard returned from this call is dropped.
    #[inline(always)]
    pub fn incr_cache_loading(&self) -> TimingGuard<'_> {
        self.exec(EventFilter::INCR_CACHE_LOADS, |profiler| {
            TimingGuard::start(
                profiler,
                profiler.incremental_load_result_event_kind,
                EventId::INVALID,
            )
        })
    }

    #[inline(always)]
    fn instant_query_event(
        &self,
        event_kind: fn(&SelfProfiler) -> StringId,
        query_invocation_id: QueryInvocationId,
        event_filter: EventFilter,
    ) {
        drop(self.exec(event_filter, |profiler| {
            let event_id = StringId::new_virtual(query_invocation_id.0);
            let thread_id = thread_id_to_u32(std::thread::current().id());

            profiler.profiler.record_instant_event(
                event_kind(profiler),
                EventId::from_virtual(event_id),
                thread_id,
            );

            TimingGuard::none()
        }));
    }

    pub fn with_profiler(&self, f: impl FnOnce(&SelfProfiler)) {
        if let Some(profiler) = &self.profiler {
            f(&profiler)
        }
    }

    #[inline]
    pub fn enabled(&self) -> bool {
        self.profiler.is_some()
    }
}

pub struct SelfProfiler {
    profiler: Profiler,
    event_filter_mask: EventFilter,

    string_cache: RwLock<FxHashMap<&'static str, StringId>>,

    query_event_kind: StringId,
    generic_activity_event_kind: StringId,
    incremental_load_result_event_kind: StringId,
    query_blocked_event_kind: StringId,
    query_cache_hit_event_kind: StringId,
}

impl SelfProfiler {
    pub fn new(
        output_directory: &Path,
        crate_name: Option<&str>,
        event_filters: &Option<Vec<String>>,
    ) -> Result<SelfProfiler, Box<dyn Error>> {
        fs::create_dir_all(output_directory)?;

        let crate_name = crate_name.unwrap_or("unknown-crate");
        let filename = format!("{}-{}.rustc_profile", crate_name, process::id());
        let path = output_directory.join(&filename);
        let profiler = Profiler::new(&path)?;

        let query_event_kind = profiler.alloc_string("Query");
        let generic_activity_event_kind = profiler.alloc_string("GenericActivity");
        let incremental_load_result_event_kind = profiler.alloc_string("IncrementalLoadResult");
        let query_blocked_event_kind = profiler.alloc_string("QueryBlocked");
        let query_cache_hit_event_kind = profiler.alloc_string("QueryCacheHit");

        let mut event_filter_mask = EventFilter::empty();

        if let Some(ref event_filters) = *event_filters {
            let mut unknown_events = vec![];
            for item in event_filters {
                if let Some(&(_, mask)) =
                    EVENT_FILTERS_BY_NAME.iter().find(|&(name, _)| name == item)
                {
                    event_filter_mask |= mask;
                } else {
                    unknown_events.push(item.clone());
                }
            }

            // Warn about any unknown event names
            if unknown_events.len() > 0 {
                unknown_events.sort();
                unknown_events.dedup();

                warn!(
                    "Unknown self-profiler events specified: {}. Available options are: {}.",
                    unknown_events.join(", "),
                    EVENT_FILTERS_BY_NAME
                        .iter()
                        .map(|&(name, _)| name.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        } else {
            event_filter_mask = EventFilter::DEFAULT;
        }

        Ok(SelfProfiler {
            profiler,
            event_filter_mask,
            string_cache: RwLock::new(FxHashMap::default()),
            query_event_kind,
            generic_activity_event_kind,
            incremental_load_result_event_kind,
            query_blocked_event_kind,
            query_cache_hit_event_kind,
        })
    }

    /// Allocates a new string in the profiling data. Does not do any caching
    /// or deduplication.
    pub fn alloc_string<STR: SerializableString + ?Sized>(&self, s: &STR) -> StringId {
        self.profiler.alloc_string(s)
    }

    /// Gets a `StringId` for the given string. This method makes sure that
    /// any strings going through it will only be allocated once in the
    /// profiling data.
    pub fn get_or_alloc_cached_string(&self, s: &'static str) -> StringId {
        // Only acquire a read-lock first since we assume that the string is
        // already present in the common case.
        {
            let string_cache = self.string_cache.read();

            if let Some(&id) = string_cache.get(s) {
                return id;
            }
        }

        let mut string_cache = self.string_cache.write();
        // Check if the string has already been added in the small time window
        // between dropping the read lock and acquiring the write lock.
        *string_cache.entry(s).or_insert_with(|| self.profiler.alloc_string(s))
    }

    pub fn map_query_invocation_id_to_string(&self, from: QueryInvocationId, to: StringId) {
        let from = StringId::new_virtual(from.0);
        self.profiler.map_virtual_to_concrete_string(from, to);
    }

    pub fn bulk_map_query_invocation_id_to_single_string<I>(&self, from: I, to: StringId)
    where
        I: Iterator<Item = QueryInvocationId> + ExactSizeIterator,
    {
        let from = from.map(|qid| StringId::new_virtual(qid.0));
        self.profiler.bulk_map_virtual_to_single_concrete_string(from, to);
    }

    pub fn query_key_recording_enabled(&self) -> bool {
        self.event_filter_mask.contains(EventFilter::QUERY_KEYS)
    }

    pub fn event_id_builder(&self) -> EventIdBuilder<'_, SerializationSink> {
        EventIdBuilder::new(&self.profiler)
    }
}

#[must_use]
pub struct TimingGuard<'a>(Option<measureme::TimingGuard<'a, SerializationSink>>);

impl<'a> TimingGuard<'a> {
    #[inline]
    pub fn start(
        profiler: &'a SelfProfiler,
        event_kind: StringId,
        event_id: EventId,
    ) -> TimingGuard<'a> {
        let thread_id = thread_id_to_u32(std::thread::current().id());
        let raw_profiler = &profiler.profiler;
        let timing_guard =
            raw_profiler.start_recording_interval_event(event_kind, event_id, thread_id);
        TimingGuard(Some(timing_guard))
    }

    #[inline]
    pub fn finish_with_query_invocation_id(self, query_invocation_id: QueryInvocationId) {
        if let Some(guard) = self.0 {
            let event_id = StringId::new_virtual(query_invocation_id.0);
            let event_id = EventId::from_virtual(event_id);
            guard.finish_with_override_event_id(event_id);
        }
    }

    #[inline]
    pub fn none() -> TimingGuard<'a> {
        TimingGuard(None)
    }
}

#[must_use]
pub struct VerboseTimingGuard<'a> {
    event_id: &'a str,
    start: Option<Instant>,
    _guard: TimingGuard<'a>,
}

impl<'a> VerboseTimingGuard<'a> {
    pub fn start(event_id: &'a str, verbose: bool, _guard: TimingGuard<'a>) -> Self {
        VerboseTimingGuard {
            event_id,
            _guard,
            start: if unlikely!(verbose) { Some(Instant::now()) } else { None },
        }
    }

    #[inline(always)]
    pub fn run<R>(self, f: impl FnOnce() -> R) -> R {
        let _timer = self;
        f()
    }
}

impl Drop for VerboseTimingGuard<'_> {
    fn drop(&mut self) {
        self.start.map(|start| print_time_passes_entry(true, self.event_id, start.elapsed()));
    }
}

pub fn print_time_passes_entry(do_it: bool, what: &str, dur: Duration) {
    if !do_it {
        return;
    }

    let mem_string = match get_resident() {
        Some(n) => {
            let mb = n as f64 / 1_000_000.0;
            format!("; rss: {}MB", mb.round() as usize)
        }
        None => String::new(),
    };
    println!("time: {}{}\t{}", duration_to_secs_str(dur), mem_string, what);
}

// Hack up our own formatting for the duration to make it easier for scripts
// to parse (always use the same number of decimal places and the same unit).
pub fn duration_to_secs_str(dur: std::time::Duration) -> String {
    const NANOS_PER_SEC: f64 = 1_000_000_000.0;
    let secs = dur.as_secs() as f64 + dur.subsec_nanos() as f64 / NANOS_PER_SEC;

    format!("{:.3}", secs)
}

// Memory reporting
#[cfg(unix)]
fn get_resident() -> Option<usize> {
    let field = 1;
    let contents = fs::read("/proc/self/statm").ok()?;
    let contents = String::from_utf8(contents).ok()?;
    let s = contents.split_whitespace().nth(field)?;
    let npages = s.parse::<usize>().ok()?;
    Some(npages * 4096)
}

#[cfg(windows)]
fn get_resident() -> Option<usize> {
    type BOOL = i32;
    type DWORD = u32;
    type HANDLE = *mut u8;
    use libc::size_t;
    #[repr(C)]
    #[allow(non_snake_case)]
    struct PROCESS_MEMORY_COUNTERS {
        cb: DWORD,
        PageFaultCount: DWORD,
        PeakWorkingSetSize: size_t,
        WorkingSetSize: size_t,
        QuotaPeakPagedPoolUsage: size_t,
        QuotaPagedPoolUsage: size_t,
        QuotaPeakNonPagedPoolUsage: size_t,
        QuotaNonPagedPoolUsage: size_t,
        PagefileUsage: size_t,
        PeakPagefileUsage: size_t,
    }
    #[allow(non_camel_case_types)]
    type PPROCESS_MEMORY_COUNTERS = *mut PROCESS_MEMORY_COUNTERS;
    #[link(name = "psapi")]
    extern "system" {
        fn GetCurrentProcess() -> HANDLE;
        fn GetProcessMemoryInfo(
            Process: HANDLE,
            ppsmemCounters: PPROCESS_MEMORY_COUNTERS,
            cb: DWORD,
        ) -> BOOL;
    }
    let mut pmc: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    pmc.cb = std::mem::size_of_val(&pmc) as DWORD;
    match unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) } {
        0 => None,
        _ => Some(pmc.WorkingSetSize as usize),
    }
}