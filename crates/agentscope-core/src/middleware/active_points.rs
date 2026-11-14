//! Middleware participation resolution, mirroring
//! `io.agentscope.core.middleware.ActivePointsResolver`.

use std::sync::Arc;

use super::middleware::{ExtensionPoint, Middleware, PointParticipants};

/// Resolves which middlewares participate at which extension points.
///
/// The declaration returned by [`Middleware::active_points`] is the single source of a
/// middleware's participation set: an explicit narrow set wins, an empty set keeps the
/// middleware registered but inactive, and the trait default (every point) means the
/// middleware was not declared.
///
/// Java additionally infers an undeclared (`null`) middleware's set from the hook methods
/// its class implements. Rust cannot inspect which hook methods a type overrides, so that
/// tier does not exist here: middlewares declare, and the default stays "every point".
pub struct ActivePointsResolver;

impl ActivePointsResolver {
    /// The points `middleware` participates in.
    ///
    /// Java returns a set, so repeated points in a declaration collapse to one: a
    /// middleware listing a point twice must not run its hook twice.
    pub fn resolve(middleware: &dyn Middleware) -> Vec<ExtensionPoint> {
        let mut points: Vec<ExtensionPoint> = Vec::new();
        for point in middleware.active_points() {
            if !points.contains(&point) {
                points.push(point);
            }
        }
        points
    }

    /// Groups `middlewares` per extension point, preserving supplied order within each
    /// point. Points without participants keep an empty bucket (Java's grouping simply
    /// has no entry for them).
    pub fn group_by_point(middlewares: &[Arc<dyn Middleware>]) -> PointParticipants {
        let mut grouped: PointParticipants = std::array::from_fn(|_| Vec::new());
        for middleware in middlewares {
            for point in Self::resolve(middleware.as_ref()) {
                grouped[point.index()].push(Arc::clone(middleware));
            }
        }
        grouped
    }
}
