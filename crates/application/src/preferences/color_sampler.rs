//! Settings color picker port and platform-independent sampling outcomes.
//!
//! @author kongweiguang

use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::AppResult;

/// One opaque RGB color selected from the desktop.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SampledColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl SampledColor {
    /// Keeps color construction centralized while allowing downstream callers to inspect channels.
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

/// A sampling request either selects a color or ends with a user-visible cancellation reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorSampleOutcome {
    Selected(SampledColor),
    Cancelled(ColorSampleCancellation),
}

/// Distinguishes explicit Escape, an elapsed deadline, and owner-request cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorSampleCancellation {
    Escape,
    Timeout,
    RequestCancelled,
}

/// On-demand desktop color sampling capability used by the settings color picker.
#[async_trait]
pub trait ScreenColorSampler: Send + Sync + 'static {
    /// Samples one pixel within the requested bound and returns cancellation as a normal outcome.
    async fn sample(
        &self,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> AppResult<ColorSampleOutcome>;
}
