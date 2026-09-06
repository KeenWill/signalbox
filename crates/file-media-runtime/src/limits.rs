//! Compiled upper bounds shared by declarations and processor supervision.

/// Hard safety ceiling; bounds prefix reads to protect broker memory and I/O.
pub const MAX_PROBE_PREFIX_BYTES: u64 = 65_536;
/// Hard safety ceiling; bounds suffix reads to protect broker memory and I/O.
pub const MAX_PROBE_SUFFIX_BYTES: u64 = 65_536;
/// Hard safety ceiling; bounds range fan-out to protect broker I/O scheduling.
pub const MAX_PROBE_RANGES: u32 = 16;
/// Hard safety ceiling; bounds aggregate probe reads to protect broker resources.
pub const MAX_PROBE_CUMULATIVE_BYTES: u64 = 262_144;
/// Hard safety ceiling; bounds one validation's aggregate source I/O.
pub const MAX_VALIDATION_SOURCE_BYTES: u64 = 1_073_741_824;
/// Hard safety ceiling; bounds one validation's exact-range fan-out.
pub const MAX_VALIDATION_RANGES: u32 = 4_096;
/// Hard safety ceiling; bounds one view's aggregate source I/O.
pub const MAX_READ_SOURCE_BYTES: u64 = 1_073_741_824;
/// Hard safety ceiling; bounds one random-access view's range fan-out.
pub const MAX_READ_RANGES: u32 = 4_096;
/// Hard safety ceiling; bounds one processor frame to protect daemon memory.
pub const MAX_PROCESSOR_FRAME_BYTES: usize = 1_048_576;
/// Hard safety ceiling; bounds serialized read options before processor framing.
pub const MAX_READ_OPTIONS_BYTES: usize = 65_536;
/// Hard safety ceiling; bounds structured JSON so nested wire escaping fits one frame.
pub const MAX_TEXT_OR_JSON_BYTES: usize = 500_000;
/// Hard safety ceiling; bounds text so worst-case JSON escaping fits one tool result.
pub const MAX_TEXT_BODY_BYTES: usize = 174_000;
/// Hard safety ceiling; bounds JSON nesting to protect recursive traversal.
pub const MAX_STRUCTURED_DEPTH: u32 = 64;
/// Hard safety ceiling; bounds JSON nodes to protect traversal work and memory.
pub const MAX_STRUCTURED_NODES: u64 = 100_000;
/// Hard safety ceiling; bounds one JSON container to protect concentrated fan-out.
pub const MAX_OBSERVED_CONTAINER_ENTRIES: u64 = 10_000;
/// Hard safety ceiling; bounds one image axis to protect decoder allocation.
pub const MAX_IMAGE_AXIS: u32 = 8_192;
/// Hard safety ceiling; bounds decoded image area to protect decoder memory.
pub const MAX_DECODED_IMAGE_PIXELS: u64 = 16_777_216;
/// Hard safety ceiling; bounds presented image payloads to protect result memory.
pub const MAX_PRESENTED_IMAGE_BYTES: u64 = 8_388_608;
/// Hard safety ceiling; bounds channel fan-out to protect decoder memory and work.
pub const MAX_AUDIO_CHANNELS: u16 = 8;
/// Hard safety ceiling; bounds samples per second to protect decoder work.
pub const MAX_AUDIO_SAMPLE_RATE_HZ: u32 = 192_000;
/// Hard safety ceiling; bounds clip duration to protect decoder work and memory.
pub const MAX_AUDIO_CLIP_SECONDS: u32 = 60;
/// Hard safety ceiling; bounds presented audio payloads to protect result memory.
pub const MAX_PRESENTED_AUDIO_BYTES: u64 = 8_388_608;
/// Hard safety ceiling; bounds presented file payloads to protect result memory.
pub const MAX_PRESENTED_FILE_BYTES: u64 = 8_388_608;
/// Maximum durable media references emitted by one model call.
pub const MAX_MEDIA_REFERENCES_PER_CALL: u16 = 16;
/// Maximum aggregate referenced media bytes emitted by one model call.
pub const MAX_AGGREGATE_MEDIA_BYTES_PER_CALL: u64 = 33_554_432;
/// Maximum isolated worker address-space bytes.
pub const MAX_WORKER_MEMORY_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum isolated worker CPU seconds.
pub const MAX_WORKER_CPU_SECONDS: u64 = 60;
/// Maximum isolated worker wall-clock seconds.
pub const MAX_WORKER_WALL_SECONDS: u64 = 120;
/// Maximum isolated worker descendants. Threads remain permitted.
pub const MAX_WORKER_DESCENDANTS: u32 = 0;
/// Maximum kernel tasks available to one isolated worker process tree.
pub const MAX_WORKER_TASKS: u64 = 64;
/// Maximum file descriptors available to an isolated worker.
pub const MAX_WORKER_FILE_DESCRIPTORS: u64 = 32;
/// Minimum descriptor ceiling that can launch bubblewrap and the dynamic worker.
pub const MIN_WORKER_FILE_DESCRIPTORS: u64 = 16;
/// Maximum retained diagnostic bytes from an isolated worker.
pub const MAX_WORKER_STDERR_BYTES: usize = 16_384;

/// Process-wide ceilings against which every declaration is checked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileMediaCeilings {
    /// Maximum prefix bytes.
    pub probe_prefix_bytes: u64,
    /// Maximum suffix bytes.
    pub probe_suffix_bytes: u64,
    /// Maximum arbitrary probe ranges.
    pub probe_ranges: u32,
    /// Maximum cumulative probe bytes.
    pub probe_cumulative_bytes: u64,
    /// Maximum cumulative source bytes for one validation.
    pub validation_source_bytes: u64,
    /// Maximum exact ranges for one validation.
    pub validation_ranges: u32,
    /// Maximum cumulative source bytes for one read view.
    pub read_source_bytes: u64,
    /// Maximum exact ranges for one random-access read view.
    pub read_ranges: u32,
    /// Maximum result body bytes.
    pub text_or_json_bytes: usize,
    /// Maximum structured nesting.
    pub structured_depth: u32,
    /// Maximum structured nodes.
    pub structured_nodes: u64,
    /// Maximum observed container entries.
    pub observed_container_entries: u64,
    /// Maximum image axis.
    pub image_axis: u32,
    /// Maximum decoded image pixels.
    pub decoded_image_pixels: u64,
    /// Maximum presented image bytes.
    pub presented_image_bytes: u64,
    /// Maximum audio channels.
    pub audio_channels: u16,
    /// Maximum audio sample rate.
    pub audio_sample_rate_hz: u32,
    /// Maximum audio duration.
    pub audio_clip_seconds: u32,
    /// Maximum presented audio bytes.
    pub presented_audio_bytes: u64,
    /// Maximum presented general-file bytes.
    pub presented_file_bytes: u64,
    /// Maximum durable media references per model call.
    pub media_references_per_call: u16,
    /// Maximum aggregate referenced media bytes per model call.
    pub aggregate_media_bytes_per_call: u64,
}

impl FileMediaCeilings {
    /// Returns the hard-coded version-one ceiling set.
    pub const fn version_one() -> Self {
        Self {
            probe_prefix_bytes: MAX_PROBE_PREFIX_BYTES,
            probe_suffix_bytes: MAX_PROBE_SUFFIX_BYTES,
            probe_ranges: MAX_PROBE_RANGES,
            probe_cumulative_bytes: MAX_PROBE_CUMULATIVE_BYTES,
            validation_source_bytes: MAX_VALIDATION_SOURCE_BYTES,
            validation_ranges: MAX_VALIDATION_RANGES,
            read_source_bytes: MAX_READ_SOURCE_BYTES,
            read_ranges: MAX_READ_RANGES,
            text_or_json_bytes: MAX_TEXT_OR_JSON_BYTES,
            structured_depth: MAX_STRUCTURED_DEPTH,
            structured_nodes: MAX_STRUCTURED_NODES,
            observed_container_entries: MAX_OBSERVED_CONTAINER_ENTRIES,
            image_axis: MAX_IMAGE_AXIS,
            decoded_image_pixels: MAX_DECODED_IMAGE_PIXELS,
            presented_image_bytes: MAX_PRESENTED_IMAGE_BYTES,
            audio_channels: MAX_AUDIO_CHANNELS,
            audio_sample_rate_hz: MAX_AUDIO_SAMPLE_RATE_HZ,
            audio_clip_seconds: MAX_AUDIO_CLIP_SECONDS,
            presented_audio_bytes: MAX_PRESENTED_AUDIO_BYTES,
            presented_file_bytes: MAX_PRESENTED_FILE_BYTES,
            media_references_per_call: MAX_MEDIA_REFERENCES_PER_CALL,
            aggregate_media_bytes_per_call: MAX_AGGREGATE_MEDIA_BYTES_PER_CALL,
        }
    }

    /// Accepts a deployment override only when every value lowers a compiled ceiling.
    pub const fn admits(self, candidate: Self) -> bool {
        candidate.probe_prefix_bytes > 0
            && candidate.probe_prefix_bytes <= self.probe_prefix_bytes
            && candidate.probe_suffix_bytes > 0
            && candidate.probe_suffix_bytes <= self.probe_suffix_bytes
            && candidate.probe_ranges > 0
            && candidate.probe_ranges <= self.probe_ranges
            && candidate.probe_cumulative_bytes > 0
            && candidate.probe_cumulative_bytes <= self.probe_cumulative_bytes
            && candidate.validation_source_bytes > 0
            && candidate.validation_source_bytes <= self.validation_source_bytes
            && candidate.validation_ranges > 0
            && candidate.validation_ranges <= self.validation_ranges
            && candidate.read_source_bytes > 0
            && candidate.read_source_bytes <= self.read_source_bytes
            && candidate.read_ranges > 0
            && candidate.read_ranges <= self.read_ranges
            && candidate.text_or_json_bytes > 0
            && candidate.text_or_json_bytes <= self.text_or_json_bytes
            && candidate.structured_depth > 0
            && candidate.structured_depth <= self.structured_depth
            && candidate.structured_nodes > 0
            && candidate.structured_nodes <= self.structured_nodes
            && candidate.observed_container_entries > 0
            && candidate.observed_container_entries <= self.observed_container_entries
            && candidate.image_axis > 0
            && candidate.image_axis <= self.image_axis
            && candidate.decoded_image_pixels > 0
            && candidate.decoded_image_pixels <= self.decoded_image_pixels
            && candidate.presented_image_bytes > 0
            && candidate.presented_image_bytes <= self.presented_image_bytes
            && candidate.audio_channels > 0
            && candidate.audio_channels <= self.audio_channels
            && candidate.audio_sample_rate_hz > 0
            && candidate.audio_sample_rate_hz <= self.audio_sample_rate_hz
            && candidate.audio_clip_seconds > 0
            && candidate.audio_clip_seconds <= self.audio_clip_seconds
            && candidate.presented_audio_bytes > 0
            && candidate.presented_audio_bytes <= self.presented_audio_bytes
            && candidate.presented_file_bytes > 0
            && candidate.presented_file_bytes <= self.presented_file_bytes
            && candidate.media_references_per_call > 0
            && candidate.media_references_per_call <= self.media_references_per_call
            && candidate.aggregate_media_bytes_per_call > 0
            && candidate.aggregate_media_bytes_per_call <= self.aggregate_media_bytes_per_call
    }
}

/// Labeled deployment overrides for lowerable worker resource limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileMediaProcessLimitOverrides {
    /// Combined worker-memory budget split between address space and writable tmpfs.
    pub memory_bytes: u64,
    /// CPU-second limit applied before worker startup.
    pub cpu_seconds: u64,
    /// Daemon wall-clock deadline in seconds.
    pub wall_seconds: u64,
    /// File-descriptor limit applied before worker startup.
    pub file_descriptors: u64,
    /// Retained, never-model-visible diagnostic byte limit.
    pub stderr_bytes: usize,
}

/// Daemon-supervised process limits with a fixed protocol frame and lowerable resources.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileMediaProcessCeilings {
    frame_bytes: usize,
    memory_bytes: u64,
    cpu_seconds: u64,
    wall_seconds: u64,
    file_descriptors: u64,
    stderr_bytes: usize,
}

impl FileMediaProcessCeilings {
    /// Returns the compiled version-one process ceiling set.
    pub const fn version_one() -> Self {
        Self {
            frame_bytes: MAX_PROCESSOR_FRAME_BYTES,
            memory_bytes: MAX_WORKER_MEMORY_BYTES,
            cpu_seconds: MAX_WORKER_CPU_SECONDS,
            wall_seconds: MAX_WORKER_WALL_SECONDS,
            file_descriptors: MAX_WORKER_FILE_DESCRIPTORS,
            stderr_bytes: MAX_WORKER_STDERR_BYTES,
        }
    }

    /// Constructs an effective set with the fixed protocol frame and lowerable resources.
    pub const fn try_lower(overrides: FileMediaProcessLimitOverrides) -> Option<Self> {
        let candidate = Self {
            frame_bytes: MAX_PROCESSOR_FRAME_BYTES,
            memory_bytes: overrides.memory_bytes,
            cpu_seconds: overrides.cpu_seconds,
            wall_seconds: overrides.wall_seconds,
            file_descriptors: overrides.file_descriptors,
            stderr_bytes: overrides.stderr_bytes,
        };
        if Self::version_one().admits(candidate) {
            Some(candidate)
        } else {
            None
        }
    }

    /// Returns whether the protocol frame remains fixed and every resource value is positive
    /// and no greater.
    pub const fn admits(self, candidate: Self) -> bool {
        candidate.frame_bytes == self.frame_bytes
            && candidate.memory_bytes > 0
            && candidate.memory_bytes <= self.memory_bytes
            && candidate.cpu_seconds > 0
            && candidate.cpu_seconds <= self.cpu_seconds
            && candidate.wall_seconds > 0
            && candidate.wall_seconds <= self.wall_seconds
            && candidate.file_descriptors >= MIN_WORKER_FILE_DESCRIPTORS
            && candidate.file_descriptors <= self.file_descriptors
            && candidate.stderr_bytes > 0
            && candidate.stderr_bytes <= self.stderr_bytes
    }

    /// Returns the fixed maximum length-delimited protocol frame bytes.
    pub const fn frame_bytes(self) -> usize {
        self.frame_bytes
    }

    /// Returns the combined worker-memory budget split between address space and writable tmpfs.
    pub const fn memory_bytes(self) -> u64 {
        self.memory_bytes
    }

    /// Returns the CPU-second limit applied before worker startup.
    pub const fn cpu_seconds(self) -> u64 {
        self.cpu_seconds
    }

    /// Returns the daemon wall-clock deadline.
    pub const fn wall_seconds(self) -> u64 {
        self.wall_seconds
    }

    /// Returns the descriptor limit applied before worker startup.
    pub const fn file_descriptors(self) -> u64 {
        self.file_descriptors
    }

    /// Returns the retained, never-model-visible diagnostic byte limit.
    pub const fn stderr_bytes(self) -> usize {
        self.stderr_bytes
    }
}

impl Default for FileMediaProcessCeilings {
    fn default() -> Self {
        Self::version_one()
    }
}

impl Default for FileMediaCeilings {
    fn default() -> Self {
        Self::version_one()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FileMediaCeilings, FileMediaProcessCeilings, FileMediaProcessLimitOverrides,
        MAX_AGGREGATE_MEDIA_BYTES_PER_CALL, MAX_MEDIA_REFERENCES_PER_CALL,
        MAX_PROCESSOR_FRAME_BYTES, MAX_WORKER_CPU_SECONDS, MAX_WORKER_FILE_DESCRIPTORS,
        MAX_WORKER_MEMORY_BYTES, MAX_WORKER_STDERR_BYTES, MAX_WORKER_WALL_SECONDS,
        MIN_WORKER_FILE_DESCRIPTORS,
    };

    /// deployment configuration can lower but never raise a compiled ceiling.
    #[test]
    fn file_media_ceiling_overrides_are_lowerable_only() {
        let media = FileMediaCeilings::version_one();
        assert_eq!(
            media.media_references_per_call,
            MAX_MEDIA_REFERENCES_PER_CALL
        );
        assert_eq!(
            media.aggregate_media_bytes_per_call,
            MAX_AGGREGATE_MEDIA_BYTES_PER_CALL
        );
        assert!(!FileMediaCeilings::version_one().admits(FileMediaCeilings {
            media_references_per_call: MAX_MEDIA_REFERENCES_PER_CALL + 1,
            ..media
        }));
        assert!(!FileMediaCeilings::version_one().admits(FileMediaCeilings {
            aggregate_media_bytes_per_call: MAX_AGGREGATE_MEDIA_BYTES_PER_CALL + 1,
            ..media
        }));
        assert_eq!(
            FileMediaProcessCeilings::try_lower(FileMediaProcessLimitOverrides {
                memory_bytes: MAX_WORKER_MEMORY_BYTES + 1,
                cpu_seconds: MAX_WORKER_CPU_SECONDS,
                wall_seconds: MAX_WORKER_WALL_SECONDS,
                file_descriptors: MAX_WORKER_FILE_DESCRIPTORS,
                stderr_bytes: MAX_WORKER_STDERR_BYTES,
            }),
            None
        );
        assert_eq!(
            FileMediaProcessCeilings::try_lower(FileMediaProcessLimitOverrides {
                memory_bytes: MAX_WORKER_MEMORY_BYTES,
                cpu_seconds: MAX_WORKER_CPU_SECONDS + 1,
                wall_seconds: MAX_WORKER_WALL_SECONDS,
                file_descriptors: MAX_WORKER_FILE_DESCRIPTORS,
                stderr_bytes: MAX_WORKER_STDERR_BYTES,
            }),
            None
        );
        assert_eq!(
            FileMediaProcessCeilings::try_lower(FileMediaProcessLimitOverrides {
                memory_bytes: MAX_WORKER_MEMORY_BYTES,
                cpu_seconds: MAX_WORKER_CPU_SECONDS,
                wall_seconds: MAX_WORKER_WALL_SECONDS + 1,
                file_descriptors: MAX_WORKER_FILE_DESCRIPTORS,
                stderr_bytes: MAX_WORKER_STDERR_BYTES,
            }),
            None
        );
        assert_eq!(
            FileMediaProcessCeilings::try_lower(FileMediaProcessLimitOverrides {
                memory_bytes: MAX_WORKER_MEMORY_BYTES,
                cpu_seconds: MAX_WORKER_CPU_SECONDS,
                wall_seconds: MAX_WORKER_WALL_SECONDS,
                file_descriptors: MAX_WORKER_FILE_DESCRIPTORS + 1,
                stderr_bytes: MAX_WORKER_STDERR_BYTES,
            }),
            None
        );
        assert_eq!(
            FileMediaProcessCeilings::try_lower(FileMediaProcessLimitOverrides {
                memory_bytes: MAX_WORKER_MEMORY_BYTES,
                cpu_seconds: MAX_WORKER_CPU_SECONDS,
                wall_seconds: MAX_WORKER_WALL_SECONDS,
                file_descriptors: MAX_WORKER_FILE_DESCRIPTORS,
                stderr_bytes: MAX_WORKER_STDERR_BYTES + 1,
            }),
            None
        );
        assert_eq!(
            FileMediaProcessCeilings::try_lower(FileMediaProcessLimitOverrides {
                memory_bytes: MAX_WORKER_MEMORY_BYTES,
                cpu_seconds: MAX_WORKER_CPU_SECONDS,
                wall_seconds: MAX_WORKER_WALL_SECONDS,
                file_descriptors: MAX_WORKER_FILE_DESCRIPTORS,
                stderr_bytes: MAX_WORKER_STDERR_BYTES,
            })
            .map(FileMediaProcessCeilings::frame_bytes),
            Some(MAX_PROCESSOR_FRAME_BYTES)
        );
    }

    #[test]
    fn process_ceiling_rejects_an_unlaunchable_descriptor_limit() {
        assert_eq!(
            FileMediaProcessCeilings::try_lower(FileMediaProcessLimitOverrides {
                memory_bytes: MAX_WORKER_MEMORY_BYTES,
                cpu_seconds: MAX_WORKER_CPU_SECONDS,
                wall_seconds: MAX_WORKER_WALL_SECONDS,
                file_descriptors: MIN_WORKER_FILE_DESCRIPTORS - 1,
                stderr_bytes: MAX_WORKER_STDERR_BYTES,
            }),
            None
        );
    }
}
