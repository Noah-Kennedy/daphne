// Copyright (c) 2023 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

use std::{collections::HashSet, ops::Range};

use async_trait::async_trait;
use prio::codec::Encode;

use crate::{
    audit_log::AuditLog,
    constants::DapMediaType,
    error::DapAbort,
    hpke::{HpkeConfig, HpkeProvider},
    messages::{BatchId, BatchSelector, HpkeConfigList, ReportId, TaskId, Time, TransitionFailure},
    metrics::{DaphneMetrics, DaphneRequestType},
    protocol::aggregator::{EarlyReportStateFetched, EarlyReportStateInitialized},
    DapAggregateShare, DapAggregateSpan, DapAggregationParam, DapError, DapGlobalConfig,
    DapRequest, DapResponse, DapTaskConfig, EarlyReportStateConsumed,
};

/// Report processor. Used by an aggregator [`DapAggregator`] when initializing an aggregation job.
#[async_trait]
pub trait DapReportProcessor: Sync {
    /// Return the time range in which a report must appear in order to be considered valid.
    fn valid_report_time_range(&self) -> Range<Time>;

    /// Initialize a sequence of reports that are in the "consumed" state by initializing VDAF
    /// preparation.
    async fn initialize_reports(
        &self,
        is_leader: bool,
        task_config: &DapTaskConfig,
        agg_param: &DapAggregationParam,
        reports: Vec<EarlyReportStateFetched>,
    ) -> Result<Vec<EarlyReportStateInitialized>, DapError>;

    /// Fetch reports from storage and store new reports if applicable.
    ///
    /// # Default behavior
    ///
    /// The default implementation of this method does not have storage. Thus any report indicated
    /// to be stored is rejected. This is good enough for aggregation tasks that do not require
    /// aggregating reports multiple times. Others, like the heavy hitters mode of Mastic, will
    /// need to override this method.
    async fn fetch_stored_reports(
        &self,
        _task_id: &TaskId,
        reports: Vec<EarlyReportStateConsumed>,
    ) -> Result<Vec<EarlyReportStateFetched>, DapError> {
        Ok(reports
            .into_iter()
            .map(|report| match report {
                EarlyReportStateConsumed::New {
                    metadata,
                    public_share,
                    input_share,
                    peer_prep_share,
                } => EarlyReportStateFetched::Ready {
                    metadata,
                    public_share,
                    input_share,
                    peer_prep_share,
                },

                // There are no reports in storage, so we have to reject.
                EarlyReportStateConsumed::Stored {
                    id,
                    peer_prep_share: _,
                } => EarlyReportStateFetched::Rejected {
                    id,
                    // TODO spec: Specify an error variant for this.
                    failure: TransitionFailure::ReportDropped,
                },

                EarlyReportStateConsumed::Rejected { id, failure } => {
                    EarlyReportStateFetched::Rejected { id, failure }
                }
            })
            .collect())
    }

    /// Mark reports in storage as rejected.
    ///
    /// # Notes
    ///
    /// It must be safe to call this method multiple times on the same input.
    ///
    /// # Default behavior
    ///
    /// The default implementation of this method does not have storage. As such, it is a no-op.
    async fn mark_stored_rejected<R: Send + Iterator<Item = (ReportId, TransitionFailure)>>(
        &self,
        _task_id: &TaskId,
        _reports: R,
    ) -> Result<(), DapError> {
        // There are no reports in storage, so this is a no-op.
        Ok(())
    }
}

#[derive(Debug)]
pub enum MergeAggShareError {
    AlreadyCollected,
    ReplaysDetected(HashSet<ReportId>),
    Other(DapError),
}

/// DAP Aggregator functionality.
#[async_trait]
pub trait DapAggregator<S: Sync>: HpkeProvider + DapReportProcessor + Sized {
    /// A refernce to a task configuration stored by the Aggregator.
    type WrappedDapTaskConfig<'a>: AsRef<DapTaskConfig> + Send
    where
        Self: 'a;

    /// Decide whether the given DAP request is authorized.
    ///
    /// If the return value is `None`, then the request is authorized. If the return value is
    /// `Some(reason)`, then the request is denied and `reason` conveys details about how the
    /// decision was reached.
    async fn unauthorized_reason(
        &self,
        task_config: &DapTaskConfig,
        req: &DapRequest<S>,
    ) -> Result<Option<String>, DapError>;

    /// Look up the DAP global configuration.
    fn get_global_config(&self) -> &DapGlobalConfig;

    /// taskprov: The VDAF verification key initializer. Used to derive the VDAF verify key for all
    /// tasks configured by this extension.
    fn taskprov_vdaf_verify_key_init(&self) -> Option<&[u8; 32]>;

    /// taskprov: The Collector's HPKE configuration used for all tasks configured by this
    /// extension.
    fn taskprov_collector_hpke_config(&self) -> Option<&HpkeConfig>;

    /// taskprov: Decide whether to opt-in or out-out of a task provisioned via taskprov.
    ///
    /// If the return value is `None`, then the decision is to opt-in. If the return value is
    /// `Some(reason)`, then the decision is to opt-out; `reason` conveys details about how the
    /// decision was reached (e.g.., the minimum batch size is too smal).
    fn taskprov_opt_out_reason(
        &self,
        task_config: &DapTaskConfig,
    ) -> Result<Option<String>, DapError>;

    /// taskprov: Configure a task. This is called after opting in. If successful, the next call to
    /// `get_task_config_for()` will return the configure task. Otherwise this call will return
    /// nothing.
    async fn taskprov_put(
        &self,
        req: &DapRequest<S>,
        task_config: DapTaskConfig,
    ) -> Result<(), DapError>;

    /// Look up the DAP task configuration for the given task ID.
    async fn get_task_config_for<'req>(
        &'req self,
        task_id: &'req TaskId,
    ) -> Result<Option<Self::WrappedDapTaskConfig<'req>>, DapError>;

    /// Get the current time (number of seconds since the beginning of UNIX time).
    fn get_current_time(&self) -> Time;

    /// Check whether the batch determined by the collect request would overlap with a previously
    /// collected batch.
    async fn is_batch_overlapping(
        &self,
        task_id: &TaskId,
        batch_sel: &BatchSelector,
    ) -> Result<bool, DapError>;

    /// Check whether the given batch ID has been observed before. This is called by the Leader
    /// (resp. Helper) in response to a CollectReq (resp. AggregateShareReq) for fixed-size tasks.
    async fn batch_exists(&self, task_id: &TaskId, batch_id: &BatchId) -> Result<bool, DapError>;

    /// Store a set of output shares and mark the corresponding reports as aggregated.
    ///
    /// If any report within a bucket has already been aggregated (is a replay) then that entire
    /// bucket must be skipped without changing any state, such that this operation is idempotent.
    ///
    /// # Returns
    ///
    /// A span with the same buckets as the input `agg_share_span` where the value is one of 3
    /// possible sets of values:
    /// - `Ok(())` if all went well and no reports were replays.
    /// - `Err(MergeAggShareError::ReplaysDetected)` if at least one report was a replay. This also
    ///                                              means no aggregate shares where merged.
    /// - `Err(MergeAggShareError::AlreadyCollected)` This span belong to an aggregate share that
    ///                                               has been collected.
    /// - `Err(MergeAggShareError::Other)` if another unrecoverable error occurred.
    async fn try_put_agg_share_span(
        &self,
        task_id: &TaskId,
        task_config: &DapTaskConfig,
        agg_share_span: DapAggregateSpan<DapAggregateShare>,
    ) -> DapAggregateSpan<Result<(), MergeAggShareError>>;

    /// Fetch the aggregate share for the given batch.
    async fn get_agg_share(
        &self,
        task_id: &TaskId,
        batch_sel: &BatchSelector,
    ) -> Result<DapAggregateShare, DapError>;

    /// Mark a batch as collected.
    async fn mark_collected(
        &self,
        task_id: &TaskId,
        batch_sel: &BatchSelector,
    ) -> Result<(), DapError>;

    /// Access the Prometheus metrics.
    fn metrics(&self) -> &dyn DaphneMetrics;

    /// Access the audit log.
    fn audit_log(&self) -> &dyn AuditLog;

    /// Return the hostname of the request URL. The value is "unspecified-host" if the URL does not
    /// indicate a hostname.
    fn host(&self) -> &str;
}

/// Handle request for the Aggregator's HPKE configuration.
pub async fn handle_hpke_config_req<S, A>(
    aggregator: &A,
    req: &DapRequest<S>,
    task_id: Option<TaskId>,
) -> Result<DapResponse, DapError>
where
    S: Sync,
    A: DapAggregator<S>,
{
    let metrics = aggregator.metrics();

    let hpke_config = aggregator
        .get_hpke_config_for(req.version, task_id.as_ref())
        .await?;

    if let Some(task_id) = task_id {
        let task_config = aggregator
            .get_task_config_for(&task_id)
            .await?
            .ok_or(DapAbort::UnrecognizedTask)?;

        // Check whether the DAP version in the request matches the task config.
        if task_config.as_ref().version != req.version {
            return Err(
                DapAbort::version_mismatch(req.version, task_config.as_ref().version).into(),
            );
        }
    }

    let payload = {
        let hpke_config_list = HpkeConfigList {
            hpke_configs: vec![hpke_config.clone()],
        };
        hpke_config_list.get_encoded().map_err(DapError::encoding)?
    };

    metrics.inbound_req_inc(DaphneRequestType::HpkeConfig);
    Ok(DapResponse {
        version: req.version,
        media_type: DapMediaType::HpkeConfigList,
        payload,
    })
}
