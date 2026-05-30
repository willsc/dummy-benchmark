//! Shared-memory SPSC ring-buffer bus + bench scaffolding.
//!
//! Two fixed-capacity ring buffers live in a single mmap'd file:
//!   * `ticks`  — feedhandler (producer) -> trading-engine (consumer)
//!   * `orders` — trading-engine (producer) -> feedhandler (consumer)
//!
//! Each ring is wait-free SPSC. Slots carry a monotonic sequence number so the
//! consumer can detect a fully-published slot without an extra fence.
//!
//! Also exposes the cross-binary bench scaffolding (CPU pinning, noise
//! threads, host probing, JSON report writer) that lets all three binaries
//! produce comparable reports for Intel-vs-AMD scenarios.

pub mod cpu;
pub mod latency;
pub mod message;
pub mod noise;
pub mod report;
pub mod ring;
pub mod shm;

pub use latency::{LatencyHistogram, Summary as LatencySummary};
pub use message::{MarketTick, OrderMsg, Side};
pub use noise::{spawn_noise, NoiseHandle};
pub use report::{host_info, write_report, HostInfo, RunConfig};
pub use ring::{RingConsumer, RingProducer, SpscRing, SLOT_PAYLOAD};
pub use shm::{ShmBus, BUS_VERSION};
