//! Scan requests.

use std::num::NonZeroU32;
use std::sync::Arc;

use clp_s_json::JsonOptions;
use clp_s_query::NormalExpr;
use clp_s_query::Query;
use clp_s_query::TimeBounds;
use clp_s_search::PlanLimits;
use clp_s_search::RecordProjection;
use clp_s_search::SearchPlanner;

use crate::ReadError;

/// What a consumer needs materialized; a session merges its sink's requirements into the request.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputRequirements {
    /// Render each selected record as JSON.
    pub is_json_needed: bool,

    /// Deliver each selected record's authoritative timestamp.
    pub is_timestamp_needed: bool,

    /// Deliver each selected record's `log_event_idx`.
    pub is_log_event_idx_needed: bool,

    /// An order the consumer requires; a conflicting request order fails the session build.
    pub required_order: Option<ResultOrder>,
}

impl OutputRequirements {
    /// # Returns
    ///
    /// Requirements that ask for nothing.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            is_json_needed: false,
            is_timestamp_needed: false,
            is_log_event_idx_needed: false,
            required_order: None,
        }
    }

    /// # Returns
    ///
    /// Requirements that ask for JSON records only.
    #[must_use]
    pub const fn json() -> Self {
        Self {
            is_json_needed: true,
            ..Self::none()
        }
    }

    /// # Returns
    ///
    /// These requirements, also asking for timestamps.
    #[must_use]
    pub const fn with_timestamps(self) -> Self {
        Self {
            is_timestamp_needed: true,
            ..self
        }
    }

    /// # Returns
    ///
    /// These requirements, also asking for `log_event_idx` values.
    #[must_use]
    pub const fn with_log_event_indices(self) -> Self {
        Self {
            is_log_event_idx_needed: true,
            ..self
        }
    }

    /// # Returns
    ///
    /// These requirements, also requiring `order`.
    #[must_use]
    pub const fn with_required_order(self, order: ResultOrder) -> Self {
        Self {
            required_order: Some(order),
            ..self
        }
    }

    /// Merges `other` into these requirements.
    ///
    /// # Returns
    ///
    /// The union of both requirements on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::InvalidArgument`] if both require different orders.
    pub fn merge(self, other: Self) -> Result<Self, ReadError> {
        let required_order = match (self.required_order, other.required_order) {
            (Some(order), Some(other_order)) if order != other_order => {
                return Err(ReadError::InvalidArgument(
                    format!("conflicting required orders {order:?} and {other_order:?}").into(),
                ));
            }
            (order, other_order) => order.or(other_order),
        };
        Ok(Self {
            is_json_needed: self.is_json_needed || other.is_json_needed,
            is_timestamp_needed: self.is_timestamp_needed || other.is_timestamp_needed,
            is_log_event_idx_needed: self.is_log_event_idx_needed || other.is_log_event_idx_needed,
            required_order,
        })
    }
}

/// Ordering and pruning applied before delivery.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResultOrder {
    /// Unspecified order (table-directory order, then row order, under one thread).
    #[default]
    Any,

    /// Archive order, then table-directory order, then row order (the C++ order).
    Scan,

    /// The `limit` records with the latest timestamps, session-wide, ordered by
    /// [`crate::LatestFirstKey`].
    LatestFirst { limit: NonZeroU32 },
}

/// What to scan and deliver; immutable and shared by every archive of a session.
#[derive(Clone, Debug)]
pub struct ScanRequest {
    query: Arc<Query>,
    time_bounds: TimeBounds,
    projection: RecordProjection,
    outputs: OutputRequirements,
    order: ResultOrder,
    json_options: JsonOptions,
}

impl ScanRequest {
    /// # Returns
    ///
    /// A builder for a request that matches every record, with full projection, no outputs and
    /// [`ResultOrder::Any`].
    #[must_use]
    pub fn builder() -> ScanRequestBuilder {
        ScanRequestBuilder {
            query: Query::match_all(),
            time_bounds: TimeBounds::default(),
            projection: RecordProjection::Full,
            outputs: OutputRequirements::none(),
            order: ResultOrder::Any,
            json_options: JsonOptions::default(),
        }
    }

    /// # Returns
    ///
    /// The query, with the time bounds folded in.
    #[must_use]
    pub const fn query(&self) -> &Arc<Query> {
        &self.query
    }

    /// # Returns
    ///
    /// The time bounds as given.
    #[must_use]
    pub const fn time_bounds(&self) -> TimeBounds {
        self.time_bounds
    }

    /// # Returns
    ///
    /// The projection.
    #[must_use]
    pub const fn projection(&self) -> &RecordProjection {
        &self.projection
    }

    /// # Returns
    ///
    /// The output requirements.
    #[must_use]
    pub const fn outputs(&self) -> OutputRequirements {
        self.outputs
    }

    /// # Returns
    ///
    /// The result order.
    #[must_use]
    pub const fn order(&self) -> ResultOrder {
        self.order
    }

    /// # Returns
    ///
    /// The JSON rendering options.
    #[must_use]
    pub const fn json_options(&self) -> JsonOptions {
        self.json_options
    }

    /// Merges a consumer's requirements into this request.
    ///
    /// # Returns
    ///
    /// The request with the union of both output requirements and the order both accept, on
    /// success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::InvalidArgument`] if `requirements` require an order this request does not
    ///   deliver.
    /// * Forwards [`OutputRequirements::merge`]'s return values on failure.
    pub(crate) fn with_requirements(
        &self,
        requirements: OutputRequirements,
    ) -> Result<Self, ReadError> {
        let outputs = self.outputs.merge(requirements)?;
        let order = match outputs.required_order {
            None => self.order,
            Some(required) if required == self.order => self.order,
            Some(ResultOrder::Scan) if ResultOrder::Any == self.order => ResultOrder::Scan,
            Some(required) => {
                return Err(ReadError::InvalidArgument(
                    format!(
                        "the consumer requires order {required:?}, but the request asks for {:?}",
                        self.order
                    )
                    .into(),
                ));
            }
        };
        Ok(Self {
            outputs,
            order,
            ..self.clone()
        })
    }

    /// # Returns
    ///
    /// Whether scans decode each selected record's authoritative timestamp: when the outputs ask
    /// for it, and always under [`ResultOrder::LatestFirst`], whose key it is.
    pub(crate) const fn is_timestamp_needed(&self) -> bool {
        self.outputs.is_timestamp_needed || matches!(self.order, ResultOrder::LatestFirst { .. })
    }

    /// # Returns
    ///
    /// Whether planning needs the search planner: anything but a query that folds to a constant
    /// with full projection.
    pub(crate) fn requires_search_planner(&self) -> bool {
        let is_constant = matches!(
            self.query().root(),
            NormalExpr::MatchAll | NormalExpr::MatchNone
        );
        !is_constant || RecordProjection::Full != *self.projection()
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The planner of this request on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`SearchPlanner::new`]'s return values on failure.
    pub(crate) fn search_planner(&self) -> Result<SearchPlanner, ReadError> {
        Ok(SearchPlanner::new(
            Arc::clone(self.query()),
            self.projection(),
            PlanLimits::default(),
        )?)
    }
}

/// Builds a [`ScanRequest`].
#[derive(Debug)]
pub struct ScanRequestBuilder {
    query: Query,
    time_bounds: TimeBounds,
    projection: RecordProjection,
    outputs: OutputRequirements,
    order: ResultOrder,
    json_options: JsonOptions,
}

impl ScanRequestBuilder {
    /// # Returns
    ///
    /// The builder with `query` as the query.
    #[must_use]
    pub fn query(self, query: Query) -> Self {
        Self { query, ..self }
    }

    /// # Returns
    ///
    /// The builder with `time_bounds` on the authoritative timestamp.
    #[must_use]
    pub fn time_bounds(self, time_bounds: TimeBounds) -> Self {
        Self {
            time_bounds,
            ..self
        }
    }

    /// # Returns
    ///
    /// The builder with `projection` as the projection.
    #[must_use]
    pub fn projection(self, projection: RecordProjection) -> Self {
        Self { projection, ..self }
    }

    /// # Returns
    ///
    /// The builder with `outputs` as the output requirements.
    #[must_use]
    pub fn outputs(self, outputs: OutputRequirements) -> Self {
        Self { outputs, ..self }
    }

    /// # Returns
    ///
    /// The builder with `order` as the result order.
    #[must_use]
    pub fn order(self, order: ResultOrder) -> Self {
        Self { order, ..self }
    }

    /// # Returns
    ///
    /// The builder with `options` as the JSON rendering options.
    #[must_use]
    pub fn json_options(self, options: JsonOptions) -> Self {
        Self {
            json_options: options,
            ..self
        }
    }

    /// Builds the request, folding the time bounds into the query.
    ///
    /// # Returns
    ///
    /// The request on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::InvalidArgument`] if the begin bound is after the end bound.
    pub fn build(self) -> Result<ScanRequest, ReadError> {
        if let (Some(begin), Some(end)) = (
            self.time_bounds.begin_epoch_ns,
            self.time_bounds.end_epoch_ns,
        ) && begin > end
        {
            return Err(ReadError::InvalidArgument(
                format!("the time bounds are inverted: begin {begin} ns is after end {end} ns")
                    .into(),
            ));
        }
        let query = if self.time_bounds.is_unbounded() {
            self.query
        } else {
            self.query.with_time_bounds(self.time_bounds)
        };
        Ok(ScanRequest {
            query: Arc::new(query),
            time_bounds: self.time_bounds,
            projection: self.projection,
            outputs: self.outputs,
            order: self.order,
            json_options: self.json_options,
        })
    }
}
