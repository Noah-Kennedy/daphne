// Copyright (c) 2024 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

use std::sync::Arc;

use axum::{extract::State, routing::post};
use daphne::{
    constants::DapMediaType,
    error::DapAbort,
    roles::{helper, DapHelper},
};
use daphne_service_utils::auth::DaphneAuth;

use super::{AxumDapResponse, DapRequestExtractor, DaphneService};

pub(super) fn add_helper_routes<A: DapHelper<DaphneAuth>>(
    router: super::Router<A>,
) -> super::Router<A>
where
    A: DapHelper<DaphneAuth> + DaphneService + Send + Sync + 'static,
{
    router
        .route("/:version/aggregate", post(handle_agg_job))
        .route("/:version/aggregate_share", post(handle_agg_share_req))
        .route(
            "/:version/tasks/:task_id/aggregation_jobs/:agg_job_id",
            post(handle_agg_job).put(handle_agg_job),
        )
        .route(
            "/:version/tasks/:task_id/aggregate_shares",
            post(handle_agg_share_req),
        )
}

async fn handle_agg_job<A>(
    State(app): State<Arc<A>>,
    DapRequestExtractor(req): DapRequestExtractor,
) -> AxumDapResponse
where
    A: DapHelper<DaphneAuth> + DaphneService + Send + Sync,
{
    AxumDapResponse::from_result(
        match req.media_type {
            DapMediaType::AggregationJobInitReq => {
                helper::handle_agg_job_init_req(&*app, &req).await
            }
            DapMediaType::AggregationJobContinueReq => {
                helper::handle_agg_job_cont_req(&*app, &req).await
            }
            m => Err(DapAbort::BadRequest(format!("unexpected media type: {m:?}")).into()),
        },
        app.server_metrics(),
    )
}

async fn handle_agg_share_req<A>(
    State(app): State<Arc<A>>,
    DapRequestExtractor(req): DapRequestExtractor,
) -> AxumDapResponse
where
    A: DapHelper<DaphneAuth> + DaphneService + Send + Sync,
{
    AxumDapResponse::from_result(
        helper::handle_agg_share_req(&*app, &req).await,
        app.server_metrics(),
    )
}