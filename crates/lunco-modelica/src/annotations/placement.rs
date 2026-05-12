//! Placement and transformation annotations.

use serde::{Deserialize, Serialize};
use super::types::{Extent, Point};

/// Decoded `Placement(transformation(...), [iconTransformation(...)])` annotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub transformation: Transformation,
}

/// `transformation(extent=..., origin=..., rotation=...)` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transformation {
    pub extent: Extent,
    /// Defaults to (0, 0) per MLS Annex D when not given.
    pub origin: Point,
    /// Degrees CCW. Defaults to 0.
    pub rotation: f64,
}
