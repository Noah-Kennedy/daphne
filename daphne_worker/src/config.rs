// Copyright (c) 2022 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

//! Daphne-Worker implements a Workers backend for Daphne.

//! Daphne-Worker configuration.

use crate::{
    auth::{DaphneWorkerAuth, DaphneWorkerAuthMethod},
    durable::{
        durable_name_report_store, durable_name_task,
        leader_batch_queue::{LeaderBatchQueueResult, DURABLE_LEADER_BATCH_QUEUE_CURRENT},
        DurableConnector, BINDING_DAP_GARBAGE_COLLECTOR, BINDING_DAP_LEADER_BATCH_QUEUE,
        DURABLE_DELETE_ALL,
    },
    error_reporting::ErrorReporter,
    int_err,
    metrics::DaphneWorkerMetrics,
    InternalTestAddTask, InternalTestEndpointForTask, InternalTestRole,
};
use daphne::{
    audit_log::AuditLog,
    auth::BearerToken,
    constants::DapMediaType,
    error::DapAbort,
    fatal_error,
    hpke::{HpkeConfig, HpkeReceiverConfig},
    messages::{
        decode_base64url_vec, AggregationJobId, BatchId, CollectionJobId, ReportMetadata, TaskId,
    },
    DapError, DapGlobalConfig, DapQueryConfig, DapRequest, DapResource, DapResponse, DapTaskConfig,
    DapVersion, Prio3Config, VdafConfig, VdafDpConfig,
};
use futures::TryFutureExt;
use matchit::Router;
use prio::{
    codec::Decode,
    dp::{Rational, ZeroConcentratedDifferentialPrivacyBudget},
    vdaf::prg::{Prg, PrgSha3, Seed, SeedStream},
};
use prometheus::{Encoder, Registry};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::HashMap,
    fmt::Display,
    io::Cursor,
    sync::{Arc, RwLock},
    time::Duration,
};
use tracing::{debug, error, info, trace, warn};
use worker::{kv::KvStore, *};

const KV_KEY_PREFIX_HPKE_RECEIVER_CONFIG_SET: &str = "hpke_receiver_config_set";
pub(crate) const KV_KEY_PREFIX_BEARER_TOKEN_LEADER: &str = "bearer_token/leader/task";
pub(crate) const KV_KEY_PREFIX_BEARER_TOKEN_COLLECTOR: &str = "bearer_token/collector/task";
pub(crate) const KV_KEY_PREFIX_TASK_CONFIG: &str = "config/task";
pub(crate) const KV_BINDING_DAP_CONFIG: &str = "DAP_CONFIG";

const DAP_BASE_URL: &str = "DAP_BASE_URL";

const INT_ERR_PEER_ABORT: &str = "request aborted by peer";
const INT_ERR_PEER_RESP_MISSING_MEDIA_TYPE: &str = "peer response is missing media type";

/// Long-lived parameters for tasks using draft-wang-ppm-dap-taskprov-02 ("taskprov").
pub(crate) struct TaskprovConfig {
    /// HPKE collector configuration for all taskprov tasks.
    pub(crate) hpke_collector_config: HpkeConfig,

    /// VDAF verify key init secret, used to generate the VDAF verification key for a taskprov task.
    pub(crate) vdaf_verify_key_init: [u8; 32],

    /// Leader, Helper: Method for authorizing Leader requests.
    pub(crate) leader_auth: DaphneWorkerAuthMethod,

    /// Leader: Method for authorizing Collector requests.
    pub(crate) collector_auth: Option<DaphneWorkerAuthMethod>,
}

/// Parameters required for pushing Prometheus metrics.
struct MetricsPushConfig {
    /// URL of the server to push metrics to.
    server: Url,

    /// Beaer token to present to the server in the HTTP request.
    bearer_token: BearerToken,
}

/// Daphne-Worker configuration, including long-lived parameters used across DAP tasks.
pub(crate) struct DaphneWorkerConfig {
    /// Indicates if DaphneWorker is used as the Leader.
    pub(crate) is_leader: bool,

    /// Global DAP configuration.
    pub(crate) global: DapGlobalConfig,

    /// Deployment type. This controls certain behavior overrides relevant to specific deployments.
    pub(crate) deployment: DaphneWorkerDeployment,

    /// Leader: Key used to derive collection job IDs. This field is not configured by the Helper.
    pub(crate) collection_job_id_key: Option<Seed<16>>,

    /// Sharding key, used to compute the ReportsPending or ReportsProcessed shard to map a report
    /// to (based on the report ID).
    report_shard_key: Seed<16>,

    /// Shard count, the number of report storage shards. This should be a power of 2.
    report_shard_count: u64,

    /// draft-dcook-ppm-dap-interop-test-design: Base URL of the Aggregator (unversioned). If set,
    /// this field is used for endpoint configuration for interop testing.
    base_url: Option<Url>,

    /// Optional: draft-wang-ppm-dap-taskprov-02 configuration. If not configured, then taskprov
    /// will be disabled.
    pub(crate) taskprov: Option<TaskprovConfig>,

    /// Default DAP version to use if not specified by the API URL
    pub(crate) default_version: DapVersion,

    /// Admin bearer token. If configured, it is used to authorize requests from the administrator.
    pub(crate) admin_token: Option<BearerToken>,

    /// Helper: Time to wait before deleting an instance of HelperStateStore. This field is not
    /// configured by the Leader.
    pub(crate) helper_state_store_garbage_collect_after_secs: Option<Duration>,

    /// Additional time to wait before deletng an instance of ReportsProcessed. Added to the value
    /// of the `report_storage_epoch_duration` field of the global DAP configuration.
    pub(crate) processed_alarm_safety_interval: Duration,

    /// Metrics push configuration.
    metrics_push_config: Option<MetricsPushConfig>,
}

impl DaphneWorkerConfig {
    pub(crate) fn from_worker_env(env: &Env) -> Result<Self> {
        let is_leader = match env.var("DAP_AGGREGATOR_ROLE")?.to_string().as_str() {
            "leader" => true,
            "helper" => false,
            other => {
                return Err(Error::RustError(format!(
                    "Invalid value for DAP_AGGREGATOR_ROLE: '{other}'",
                )))
            }
        };

        let global: DapGlobalConfig =
            serde_json::from_str(env.var("DAP_GLOBAL_CONFIG")?.to_string().as_ref())
                .map_err(|e| Error::RustError(format!("Failed to parse DAP_GLOBAL_CONFIG: {e}")))?;

        let default_version =
            DapVersion::from(env.var("DAP_DEFAULT_VERSION")?.to_string().as_ref());

        let base_url = if let Ok(base_url) = env.var(DAP_BASE_URL) {
            let base_url: Url = base_url
                .to_string()
                .parse()
                .map_err(|e| Error::RustError(format!("failed to parse {DAP_BASE_URL}: {e}")))?;
            Some(base_url)
        } else {
            None
        };

        const DAP_COLLECTION_JOB_ID_KEY: &str = "DAP_COLLECTION_JOB_ID_KEY";
        let collection_job_id_key = if is_leader {
            let collection_job_id_key_hex = env
                .secret(DAP_COLLECTION_JOB_ID_KEY)
                .map_err(|e| format!("failed to load {DAP_COLLECTION_JOB_ID_KEY}: {e}"))?
                .to_string();
            let collection_job_id_key =
                Seed::get_decoded(&hex::decode(collection_job_id_key_hex).map_err(int_err)?)
                    .map_err(int_err)?;
            Some(collection_job_id_key)
        } else {
            None
        };

        let report_shard_key = Seed::get_decoded(
            &hex::decode(env.secret("DAP_REPORT_SHARD_KEY")?.to_string()).map_err(int_err)?,
        )
        .map_err(int_err)?;

        let report_shard_count: u64 = env
            .var("DAP_REPORT_SHARD_COUNT")?
            .to_string()
            .parse()
            .map_err(|err| {
                Error::RustError(format!("Failed to parse DAP_REPORT_SHARD_COUNT: {err}"))
            })?;

        let deployment = if let Ok(deployment) = env.var("DAP_DEPLOYMENT") {
            match deployment.to_string().as_str() {
                "prod" => DaphneWorkerDeployment::Prod,
                "dev" => DaphneWorkerDeployment::Dev,
                s => {
                    return Err(Error::RustError(format!(
                        "Invalid value for DAP_DEPLOYMENT: {s}",
                    )))
                }
            }
        } else {
            DaphneWorkerDeployment::default()
        };
        if !matches!(deployment, DaphneWorkerDeployment::Prod) {
            trace!("DAP deployment override applied: {deployment:?}");
        }

        let taskprov = if global.allow_taskprov {
            let hpke_collector_config = serde_json::from_str(
                env.var("DAP_TASKPROV_HPKE_COLLECTOR_CONFIG")?
                    .to_string()
                    .as_ref(),
            )?;

            const DAP_TASKPROV_VDAF_VERIFY_KEY_INIT: &str = "DAP_TASKPROV_VDAF_VERIFY_KEY_INIT";
            let vdaf_verify_key_init =
                hex::decode(env.secret(DAP_TASKPROV_VDAF_VERIFY_KEY_INIT)?.to_string())
                    .map_err(|e| {
                        Error::RustError(format!(
                            "{DAP_TASKPROV_VDAF_VERIFY_KEY_INIT}: Failed to decode hex: {e}"
                        ))
                    })?
                    .try_into()
                    .map_err(|_| {
                        Error::RustError(format!(
                            "{DAP_TASKPROV_VDAF_VERIFY_KEY_INIT}: Incorrect length"
                        ))
                    })?;

            const DAP_TASKPROV_LEADER_AUTH: &str = "DAP_TASKPROV_LEADER_AUTH";
            let leader_auth =
                serde_json::from_str(env.var(DAP_TASKPROV_LEADER_AUTH)?.to_string().as_ref())
                    .map_err(|e| {
                        error!("DaphneWorkerConfig: error parsing {DAP_TASKPROV_LEADER_AUTH}: {e}");
                        e
                    })?;

            let collector_auth = if is_leader {
                const DAP_TASKPROV_COLLECTOR_AUTH: &str = "DAP_TASKPROV_COLLECTOR_AUTH";
                Some(
                    serde_json::from_str(
                        env.var(DAP_TASKPROV_COLLECTOR_AUTH)?.to_string().as_ref(),
                    )
                    .map_err(|e| {
                        error!(
                            "DaphneWorkerConfig: error parsing {DAP_TASKPROV_COLLECTOR_AUTH}: {e}"
                        );
                        e
                    })?,
                )
            } else {
                None
            };

            Some(TaskprovConfig {
                hpke_collector_config,
                vdaf_verify_key_init,
                leader_auth,
                collector_auth,
            })
        } else {
            None
        };

        let admin_token = match env.secret("DAP_ADMIN_BEARER_TOKEN") {
            Ok(raw) => Some(BearerToken::from(raw.to_string())),
            Err(err) => {
                trace!("DAP_ADMIN_BEARER_TOKEN not configured: {err:?}");
                None
            }
        };

        let helper_state_store_garbage_collect_after_secs = if !is_leader {
            Some(Duration::from_secs(
                env.var("DAP_HELPER_STATE_STORE_GARBAGE_COLLECT_AFTER_SECS")?
                    .to_string()
                    .parse()
                    .map_err(|err| {
                        Error::RustError(format!(
                            "Failed to parse DAP_HELPER_STATE_STORE_GARBAGE_COLLECT_AFTER_SECS: {err}"
                        ))
                    })?,
            ))
        } else {
            None
        };

        let processed_alarm_safety_interval = Duration::from_secs(
            env.var("DAP_PROCESSED_ALARM_SAFETY_INTERVAL")?
                .to_string()
                .parse()
                .map_err(|err| {
                    Error::RustError(format!(
                        "Failed to parse DAP_PROCESSED_ALARM_SAFETY_INTERVAL: {err}"
                    ))
                })?,
        );

        const DAP_METRICS_PUSH_SERVER_URL: &str = "DAP_METRICS_PUSH_SERVER_URL";
        const DAP_METRICS_PUSH_BEARER_TOKEN: &str = "DAP_METRICS_PUSH_BEARER_TOKEN";
        let metrics_push_config = match (
            env.var(DAP_METRICS_PUSH_SERVER_URL),
            env.var(DAP_METRICS_PUSH_BEARER_TOKEN),
        ) {
            (Ok(server_str), Ok(bearer_token_str)) => Some(MetricsPushConfig {
                server: server_str.to_string().parse().map_err(|err| {
                    Error::RustError(format!(
                        "Failed to parse {DAP_METRICS_PUSH_SERVER_URL}: {err:?}"
                    ))
                })?,
                bearer_token: BearerToken::from(bearer_token_str.to_string()),
            }),
            (Err(..), Err(..)) => None,
            (Ok(..), Err(..)) => {
                return Err(Error::RustError(
                    "failed to configure metrics push: missing bearer token".into(),
                ))
            }
            (Err(..), Ok(..)) => {
                return Err(Error::RustError(
                    "failed to configure metrics push: missing server URL".into(),
                ))
            }
        };

        Ok(Self {
            global,
            deployment,
            collection_job_id_key,
            report_shard_key,
            report_shard_count,
            base_url,
            is_leader,
            taskprov,
            default_version,
            admin_token,
            helper_state_store_garbage_collect_after_secs,
            processed_alarm_safety_interval,
            metrics_push_config,
        })
    }

    /// Derive the batch name for a report for the given task and with the given report ID.
    pub(crate) fn durable_name_report_store(
        &self,
        task_config: &DapTaskConfig,
        task_id_hex: &str,
        metadata: &ReportMetadata,
    ) -> String {
        let mut shard_seed = [0; 8];
        PrgSha3::seed_stream(
            &self.report_shard_key,
            b"report shard",
            metadata.id.as_ref(),
        )
        .fill(&mut shard_seed);
        let shard = u64::from_be_bytes(shard_seed) % self.report_shard_count;
        let epoch = metadata.time - (metadata.time % self.global.report_storage_epoch_duration);
        durable_name_report_store(&task_config.version, task_id_hex, epoch, shard)
    }
}

pub(crate) type HpkeRecieverConfigList = Vec<HpkeReceiverConfig>;

/// Daphne-Worker per-isolate state, which may be used by multiple requests. Includes long-lived configuration,
/// cached responses from KV, etc.
pub(crate) struct DaphneWorkerIsolateState {
    pub(crate) config: DaphneWorkerConfig,

    /// HTTP client to use for making requests.
    pub(crate) client: reqwest_wasm::Client,

    /// Cached HPKE receiver config. This will be populated when Daphne-Worker obtains an HPKE
    /// receiver config for the first time from Cloudflare KV.
    hpke_receiver_configs: Arc<RwLock<HashMap<DapVersion, HpkeRecieverConfigList>>>,

    /// Laeder bearer token per task.
    pub(crate) leader_bearer_tokens: Arc<RwLock<HashMap<TaskId, BearerToken>>>,

    /// Collector bearer token per task.
    collector_bearer_tokens: Arc<RwLock<HashMap<TaskId, BearerToken>>>,

    /// Task list.
    pub(crate) tasks: Arc<RwLock<HashMap<TaskId, DapTaskConfig>>>,
}

impl DaphneWorkerIsolateState {
    pub(crate) fn from_worker_env(env: &Env) -> Result<Self> {
        let config = DaphneWorkerConfig::from_worker_env(env)?;

        // TODO Configure this client to use HTTPS only, except if running in a test environment.
        let client = reqwest_wasm::Client::new();

        Ok(Self {
            config,
            client,
            hpke_receiver_configs: Arc::new(RwLock::new(HashMap::new())),
            leader_bearer_tokens: Arc::new(RwLock::new(HashMap::new())),
            collector_bearer_tokens: Arc::new(RwLock::new(HashMap::new())),
            tasks: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    fn delete_all(&self) -> std::result::Result<(), DapError> {
        macro_rules! clear_guarded_map {
            ($name:ident) => {{
                let mut guarded_map = self.$name.write().map_err(|e| {
                    fatal_error!(err = format!("Failed to lock {} for writing: {e}", "$name"))
                })?;
                guarded_map.clear();
            }};
        }

        clear_guarded_map!(hpke_receiver_configs);
        clear_guarded_map!(leader_bearer_tokens);
        clear_guarded_map!(collector_bearer_tokens);
        clear_guarded_map!(tasks);
        Ok(())
    }
}

/// Daphne-Worker per-request state.
pub(crate) struct DaphneWorkerRequestState<'srv> {
    pub(crate) isolate_state: &'srv DaphneWorkerIsolateState,

    /// Registry for Prometheus metrics collected while handling the request.
    #[allow(dead_code)]
    pub(crate) prometheus_registry: Registry,

    /// Metrics.
    pub(crate) metrics: DaphneWorkerMetrics,

    /// Hostname parsed from the HTTP request URL. Set to "unspecified-daphne-worker-hsot" if the
    /// hostname is not part of the URL.
    pub(crate) host: String,

    /// Error reporting for Daphne internal errors.
    pub(crate) error_reporter: &'srv dyn ErrorReporter,

    /// Audit logging
    pub(crate) audit_log: &'srv dyn AuditLog,
}

impl<'srv> DaphneWorkerRequestState<'srv> {
    pub(crate) fn new(
        isolate_state: &'srv DaphneWorkerIsolateState,
        req: &Request,
        error_reporter: &'srv dyn ErrorReporter,
        audit_log: &'srv dyn AuditLog,
    ) -> Result<Self> {
        let prometheus_registry = Registry::new();
        let metrics = DaphneWorkerMetrics::register(&prometheus_registry, None)
            .map_err(|e| Error::RustError(format!("failed to register metrics: {e}")))?;

        crate::tracing_utils::initialize_timing_histograms(&prometheus_registry, None)
            .map_err(|e| Error::RustError(format!("failed to register metrics: {e}")))?;
        let host = req
            .url()?
            .host_str()
            .unwrap_or("unspecified-daphne-worker-host")
            .to_string();

        Ok(Self {
            isolate_state,
            prometheus_registry,
            metrics,
            host,
            error_reporter,
            audit_log,
        })
    }

    pub(crate) fn handler(&'srv self, env: &'srv Env) -> DaphneWorker<'srv> {
        DaphneWorker { state: self, env }
    }

    /// If configured, gather metrics and push to Prometheus server.
    pub(crate) async fn maybe_push_metrics(&self) -> Result<()> {
        // Prepare text exposition of metrics.
        let mut buf = Vec::new();
        let encoder = prometheus::TextEncoder::new();
        let metrics_familes = self.prometheus_registry.gather();
        encoder
            .encode(&metrics_familes, &mut buf)
            .expect("failed to encode metrics");
        let text_metrics = String::from_utf8(buf).expect("text encoding of metrics is not UTF8");
        trace!("Prometheus summary:\n{text_metrics}");

        if let Some(ref metrics_push_config) = self.isolate_state.config.metrics_push_config {
            // Prepare authorization.
            let mut headers = reqwest_wasm::header::HeaderMap::new();
            let bearer_token: &str = metrics_push_config.bearer_token.as_ref();
            headers.insert(
                reqwest_wasm::header::HeaderName::from_static("authorization"),
                reqwest_wasm::header::HeaderValue::from_str(&format!("Bearer {bearer_token}"))
                    .expect("header value malformed"),
            );

            let reqwest_resp = self
                .isolate_state
                .client
                .post(metrics_push_config.server.as_str())
                .body(text_metrics)
                .headers(headers)
                .send()
                .await
                .map_err(|err| {
                    Error::RustError(format!("request to metrics server failed: {err:?}"))
                })?;

            let status = reqwest_resp.status();
            if status != 200 {
                error!("unexpected response from metrics server: {reqwest_resp:?}");
            }
        }
        Ok(())
    }

    pub(crate) fn dap_abort_to_worker_response(&self, e: DapAbort) -> Result<Response> {
        let status = if matches!(e, DapAbort::Internal(..)) {
            self.error_reporter.report_abort(&e);
            500
        } else {
            400
        };
        self.metrics
            .dap_abort_counter
            .with_label_values(&[&self.host])
            .inc();
        let problem_details = e.into_problem_details();
        error!(
            "request aborted: {}",
            serde_json::to_string(&problem_details)?
        );
        let mut headers = Headers::new();
        headers.set("Content-Type", "application/problem+json")?;
        Ok(Response::from_json(&problem_details)?
            .with_status(status)
            .with_headers(headers))
    }
}

/// Daphne-Worker, used to handle a DAP request. Constructed from `DaphneWorkerState::handler()`.
pub(crate) struct DaphneWorker<'srv> {
    pub(crate) state: &'srv DaphneWorkerRequestState<'srv>,
    env: &'srv Env,
}

impl<'srv> DaphneWorker<'srv> {
    pub(crate) fn durable(&self) -> DurableConnector<'_> {
        DurableConnector::new(self.env)
    }

    pub(crate) fn kv(&self) -> Result<KvStore> {
        self.env.kv(KV_BINDING_DAP_CONFIG)
    }

    pub(crate) fn config(&'srv self) -> &'srv DaphneWorkerConfig {
        &self.state.isolate_state.config
    }

    pub(crate) fn isolate_state(&'srv self) -> &'srv DaphneWorkerIsolateState {
        self.state.isolate_state
    }

    /// Set a key/value pair unless the key already exists. If the key exists, then return the current
    /// value. Otherwise return nothing.
    async fn kv_set_if_not_exists<K, V>(
        &self,
        kv_key_prefix: &str,
        kv_key_suffix: &K,
        kv_value: V,
    ) -> Result<Option<V>>
    where
        K: ToString,
        V: for<'de> Deserialize<'de> + Serialize,
    {
        let kv_key = format!("{}/{}", kv_key_prefix, kv_key_suffix.to_string());
        let kv_store = self.kv()?;
        let builder = kv_store.get(&kv_key);
        let res: Option<V> = builder.json().await?;
        if res.is_some() {
            return Ok(res);
        }

        kv_store.put(&kv_key, kv_value)?.execute().await?;
        Ok(None)
    }

    async fn kv_get_cached_mapped<'req, K, V, R>(
        &self,
        map: &'srv Arc<RwLock<HashMap<K, V>>>,
        kv_key_prefix: &str,
        kv_key_suffix: Cow<'req, K>,
        mapper: impl FnOnce(KvPair<'req, K, &V>) -> R,
    ) -> Result<Option<R>>
    where
        K: Clone + Eq + std::hash::Hash + Display,
        V: for<'de> Deserialize<'de>,
    {
        // If the value is cached, then return immediately.
        {
            let guarded_map = map
                .read()
                .map_err(|e| Error::RustError(format!("Failed to lock map for reading: {e}")))?;

            if let Some(value) = guarded_map.get(&kv_key_suffix) {
                tracing::debug!(%kv_key_suffix, "found kv value in cache");
                return Ok(Some(mapper(KvPair {
                    value,
                    key: kv_key_suffix,
                })));
            }
        }

        // If the value is not cached, try to populate it from KV before returning.
        let kv_key = format!("{}/{}", kv_key_prefix, kv_key_suffix);

        tracing::debug!(%kv_key, "looking up key in kv");
        let kv_store = self.kv()?;
        let builder = kv_store.get(&kv_key);
        if let Some(kv_value) = builder.json::<V>().await? {
            // TODO(cjpatton) Consider indicating whether the value is known to not exist. For HPKE
            // configs, this would avoid hitting KV multiple times when the same expired config is
            // used for multiple reports.
            let mut guarded_map = map
                .write()
                .map_err(|e| Error::RustError(format!("Failed to lock map for writing: {e}")))?;
            guarded_map.insert(kv_key_suffix.clone().into_owned(), kv_value);
        }

        let guarded_map = map
            .read()
            .map_err(|e| Error::RustError(format!("Failed to lock map for reading: {e}")))?;

        if let Some(value) = guarded_map.get(kv_key_suffix.as_ref()) {
            tracing::debug!(%kv_key, "found key in kv");
            Ok(Some(mapper(KvPair {
                value,
                key: kv_key_suffix,
            })))
        } else {
            Ok(None)
        }
    }

    /// In memory cache on-top of KV to avoid hitting KV API limits.
    // NOTE: We shouldn't return guards from this function as they could be held across await points.
    //
    // Locks in wasm are assumed to be single threaded without concurrency:
    // https://github.com/rust-lang/rust/blob/6f8c0557e0b73c73a8a7163a15f4a5a3feca7d5c/library/std/src/sys/unsupported/locks/rwlock.rs#L4
    //
    // However, with Cloudflare Workers the situation is not so straightforward.
    //
    // A single isolate can process multiple requests concurrently (not in parallel), so holding
    // ReadGuards across await points is not OK.
    //
    // There are some official docs that refer to the behaviour:
    // https://developers.cloudflare.com/workers/learning/how-workers-works/#distributed-execution
    //
    // The errors seen are:
    //
    //   Error: The script will never generate a response.
    //
    //   and
    //
    //   Memory access out of bounds
    //
    // They only seem to happen under load, likely because the Workers runtime will readily spawn
    // additional isolates.
    async fn kv_get_cached<'req, K, V>(
        &self,
        map: &'srv Arc<RwLock<HashMap<K, V>>>,
        kv_key_prefix: &str,
        kv_key_suffix: Cow<'req, K>,
    ) -> Result<Option<KvPair<'req, K, V>>>
    where
        K: Clone + Eq + std::hash::Hash + Display,
        V: Clone + for<'de> Deserialize<'de>,
    {
        self.kv_get_cached_mapped(map, kv_key_prefix, kv_key_suffix, |pair| KvPair {
            value: pair.value.clone(),
            key: pair.key,
        })
        .await
    }

    /// Get a reference to the HPKE receiver configs, ensuring that the config indicated by
    /// `version` is cached (if it exists).
    ///
    /// The `mapper` let's you extract the minimum you need from the [`HpkeRecieverConfigList`],
    /// the goal being that you can clone as little as possible.
    ///
    /// If the `mapper` returns [`None`], then kv will be hit in order to make sure that we don't
    /// miss a new value due to having a stale cache.
    pub(crate) async fn get_hpke_receiver_config<F, R>(
        &self,
        version: DapVersion,
        mut mapper: F,
    ) -> Result<Option<R>>
    where
        F: FnMut(&HpkeRecieverConfigList) -> Option<R>,
    {
        let cached_config = self
            .isolate_state()
            .hpke_receiver_configs
            .read()
            .unwrap()
            .get(&version)
            .and_then(&mut mapper);
        match cached_config {
            Some(config) => Ok(Some(config)),
            None => Ok(self
                .kv_get_cached_mapped(
                    &self.isolate_state().hpke_receiver_configs,
                    KV_KEY_PREFIX_HPKE_RECEIVER_CONFIG_SET,
                    Cow::Owned(version),
                    |pair| mapper(pair.value),
                )
                .await?
                .flatten()),
        }
    }

    /// Retrieve from KV the Leader's bearer token for the given task.
    pub(crate) async fn get_leader_bearer_token<'a>(
        &'a self,
        task_id: &'a TaskId,
    ) -> Result<Option<BearerTokenKvPair<'a>>> {
        self.kv_get_cached(
            &self.isolate_state().leader_bearer_tokens,
            KV_KEY_PREFIX_BEARER_TOKEN_LEADER,
            Cow::Borrowed(task_id),
        )
        .await
    }

    /// Set a leader bearer token for the given task.
    pub(crate) async fn set_leader_bearer_token(
        &self,
        task_id: &TaskId,
        token: &BearerToken,
    ) -> Result<Option<BearerToken>> {
        self.kv_set_if_not_exists(KV_KEY_PREFIX_BEARER_TOKEN_LEADER, task_id, token.clone())
            .await
    }

    /// Retrieve from KV the Collector's bearer token for the given task.
    pub(crate) async fn get_collector_bearer_token<'a>(
        &'a self,
        task_id: &'a TaskId,
    ) -> Result<Option<BearerTokenKvPair>> {
        self.kv_get_cached(
            &self.isolate_state().collector_bearer_tokens,
            KV_KEY_PREFIX_BEARER_TOKEN_COLLECTOR,
            Cow::Borrowed(task_id),
        )
        .await
    }

    /// Retrieve from KV the configuration for the given task.
    pub(crate) async fn get_task_config<'req>(
        &self,
        task_id: Cow<'req, TaskId>,
    ) -> Result<Option<DapTaskConfigKvPair<'req>>> {
        self.kv_get_cached(
            &self.isolate_state().tasks,
            KV_KEY_PREFIX_TASK_CONFIG,
            task_id,
        )
        .await
    }

    /// Define a task in KV
    pub(crate) async fn set_task_config(
        &self,
        task_id: &TaskId,
        task_config: &DapTaskConfig,
    ) -> Result<Option<DapTaskConfig>> {
        self.kv_set_if_not_exists(KV_KEY_PREFIX_TASK_CONFIG, task_id, task_config.clone())
            .await
    }

    /// Try retrieving from KV the configuration for the given task. Return an error if the
    /// indicated task is not recognized.
    pub(crate) async fn try_get_task_config<'req>(
        &'srv self,
        task_id: &'req TaskId,
    ) -> std::result::Result<DapTaskConfigKvPair<'req>, DapError>
    where
        'srv: 'req,
    {
        self.get_task_config(Cow::Borrowed(task_id))
            .await
            .map_err(|e| fatal_error!(err = e, "getting task config"))?
            .ok_or(DapError::Abort(DapAbort::UnrecognizedTask))
    }

    /// Clear all persistant durable objects storage.
    ///
    /// TODO(cjpatton) Gate this to non-prod deployments. (Prod should do migration.)
    pub(crate) async fn internal_delete_all(&self) -> std::result::Result<(), DapError> {
        // Clear KV storage.
        let kv_store = self.kv().map_err(|e| fatal_error!(err = e))?;
        let kv_task = async {
            for kv_key in kv_store
                .list()
                .execute()
                .await
                .map_err(|e| fatal_error!(err = e, "failed to list all keys from kv"))?
                .keys
            {
                kv_store.delete(kv_key.name.as_str()).await.map_err(
                    |e| fatal_error!(err = e, name = %kv_key.name, "failed to delete key from kv"),
                )?;
                trace!("deleted KV item {}", kv_key.name);
            }
            Ok::<_, DapError>(())
        };

        // Clear DO storage.
        let durable = self.durable();

        let future_delete_durable = durable
            .post::<_, ()>(
                BINDING_DAP_GARBAGE_COLLECTOR,
                DURABLE_DELETE_ALL,
                "garbage_collector".to_string(),
                &(),
            )
            .map_err(|e| fatal_error!(err = e));

        futures::try_join!(kv_task, future_delete_durable)?;

        // Clear the isolate state.
        self.isolate_state().delete_all()?;

        Ok(())
    }

    /// Get the batch ID for the oldest batch that has not been collected. This method is only
    /// applicable to fixed-size tasks.
    pub(crate) async fn internal_current_batch(
        &self,
        task_id: &TaskId,
    ) -> std::result::Result<BatchId, DapError> {
        let task_config = self.try_get_task_config(task_id).await?;
        if !matches!(task_config.as_ref().query, DapQueryConfig::FixedSize { .. }) {
            return Err(fatal_error!(err = "query type mismatch"));
        }

        let res: LeaderBatchQueueResult = self
            .durable()
            .get(
                BINDING_DAP_LEADER_BATCH_QUEUE,
                DURABLE_LEADER_BATCH_QUEUE_CURRENT,
                durable_name_task(&task_config.as_ref().version, &task_id.to_hex()),
            )
            .await
            .map_err(|e| fatal_error!(err = e))?;

        match res {
            LeaderBatchQueueResult::Ok(batch_id) => Ok(batch_id),

            // TODO spec: If we end up taking the current batch semantics of
            // https://github.com/ietf-wg-ppm/draft-ietf-ppm-dap/pull/313, then we'll need to
            // define an error type for this case.
            LeaderBatchQueueResult::EmptyQueue => Err(fatal_error!(err = "empty batch queue")),
        }
    }

    /// Get the URL to use for this endpoint, as required by
    /// draft-dcook-ppm-dap-interop-test-design-02.
    pub(crate) async fn internal_endpoint_for_task(
        &self,
        version: DapVersion,
        cmd: InternalTestEndpointForTask,
    ) -> Result<Response> {
        if self.config().is_leader && !matches!(cmd.role, InternalTestRole::Leader)
            || !self.config().is_leader && !matches!(cmd.role, InternalTestRole::Helper)
        {
            return Response::from_json(&serde_json::json!({
                "status": "error",
                "error": "role mismatch",
            }));
        }

        let path = self
            .config()
            .base_url
            .as_ref()
            .ok_or_else(|| {
                Error::RustError(format!(
                    "Environment variable {DAP_BASE_URL} not configured"
                ))
            })?
            .path();

        Response::from_json(&serde_json::json!({
            "status": "success",
            "endpoint": format!("{path}{}/", version.as_ref()),
        }))
    }

    /// Configure Daphne-Worker a task, as required by draft-dcook-ppm-dap-interop-test-design-02.
    pub(crate) async fn internal_add_task(
        &self,
        version: DapVersion,
        cmd: InternalTestAddTask,
    ) -> Result<()> {
        // Task ID.
        let task_id = TaskId::try_from_base64url(&cmd.task_id)
            .ok_or_else(|| int_err("task ID is not valid URL-safe base64"))?;

        // VDAF config.
        let vdaf = match (cmd.vdaf.typ.as_ref(), cmd.vdaf.bits, cmd.vdaf.length) {
            ("Prio3Count", None, None) => VdafConfig::Prio3(Prio3Config::Count),
            ("Prio3Sum", Some(bits), None) => {
                let bits = bits.parse().map_err(int_err)?;
                VdafConfig::Prio3(Prio3Config::Sum { bits })
            }
            ("Prio3SumVec", Some(bits), Some(length)) => {
                let bits = bits.parse().map_err(int_err)?;
                let len = length.parse().map_err(int_err)?;
                VdafConfig::Prio3(Prio3Config::SumVec { bits, len })
            }
            // NOTE: the VDAF-06 format is not supported yet
            // https://datatracker.ietf.org/doc/html/draft-dcook-ppm-dap-interop-test-design-04#name-internal-test-add_task
            // Issue tracked here: https://github.com/divergentdave/draft-dcook-ppm-dap-interop-test-design/issues/40
            ("Prio3Histogram", Some(len), None) => {
                let len = len.parse().map_err(int_err)?;
                VdafConfig::Prio3(Prio3Config::Histogram { len })
            }
            _ => return Err(int_err("command failed: unrecognized VDAF")),
        };

        warn!("vdaf config: {:?}", vdaf);

        // TODO(tholop): we can run the compatibility check here too.
        let dp_config = match (
            cmd.dp_config.distribution.as_deref(),
            cmd.dp_config.zcdp_numerator,
            cmd.dp_config.zcdp_denominator,
        ) {
            (Some("Gaussian"), Some(n), Some(d)) => {
                // let n: BigUint = n.parse().map_err(int_err)?;
                // let d: BigUint = d.parse().map_err(int_err)?;
                // TODO(tholop): we could also use Rational without the from u128 method.

                let n: u128 = n.parse().map_err(int_err)?;
                let d: u128 = d.parse().map_err(int_err)?;
                let epsilon = Rational::from_unsigned(n, d).map_err(int_err)?;

                let zcdp_budget = ZeroConcentratedDifferentialPrivacyBudget::new(epsilon);
                debug!("got zcdp budget");
                Some(VdafDpConfig {
                    distribution: daphne::DistributionCodepoint::Gaussian,
                    budget: daphne::DpBudget::ZCdp { zcdp_budget },
                })
            }
            _ => {
                debug!("no valid dp config");
                None
            }
        };

        // VDAF verification key.
        let vdaf_verify_key_data = decode_base64url_vec(cmd.vdaf_verify_key.as_bytes())
            .ok_or_else(|| int_err("VDAF verify key is not valid URL-safe base64"))?;
        let vdaf_verify_key = vdaf
            .get_decoded_verify_key(&vdaf_verify_key_data)
            .map_err(int_err)?;

        // Collector HPKE config.
        let collector_hpke_config_data = decode_base64url_vec(cmd.collector_hpke_config.as_bytes())
            .ok_or_else(|| int_err("HPKE collector config is not valid URL-safe base64"))?;
        let collector_hpke_config =
            HpkeConfig::get_decoded(&collector_hpke_config_data).map_err(int_err)?;

        // Leader authentication token.
        let token = BearerToken::from(cmd.leader_authentication_token);
        if self
            .kv_set_if_not_exists(KV_KEY_PREFIX_BEARER_TOKEN_LEADER, &task_id, token)
            .await?
            .is_some()
        {
            return Err(int_err(format!(
                "command failed: token already exists for the given task ({}) and bearer role (leader)",
                cmd.task_id
            )));
        }

        // Collector authentication token.
        match (cmd.role, cmd.collector_authentication_token) {
            (InternalTestRole::Leader, Some(token_string)) => {
                let token = BearerToken::from(token_string);
                if self
                    .kv_set_if_not_exists(KV_KEY_PREFIX_BEARER_TOKEN_COLLECTOR, &task_id, token)
                    .await?
                    .is_some()
                {
                    return Err(int_err(format!(
                        "command failed: token already exists for the given task ({}) and bearer role (collector)",
                        cmd.task_id
                    )));
                }
            }
            (InternalTestRole::Leader, None) => {
                return Err(int_err(
                    "command failed: missing collector authentication token",
                ))
            }
            (InternalTestRole::Helper, None) => (),
            (InternalTestRole::Helper, Some(..)) => {
                return Err(int_err(
                    "command failed: unexpected collector authentication token",
                ));
            }
        };

        // Query configuraiton.
        let query = match (cmd.query_type, cmd.max_batch_size) {
            (1, None) => DapQueryConfig::TimeInterval,
            (1, Some(..)) => return Err(int_err("command failed: unexpected max batch size")),
            (2, Some(max_batch_size)) => DapQueryConfig::FixedSize { max_batch_size },
            (2, None) => return Err(int_err("command failed: missing max batch size")),
            _ => return Err(int_err("command failed: unrecognized query type")),
        };

        if self
            .kv_set_if_not_exists(
                KV_KEY_PREFIX_TASK_CONFIG,
                &task_id,
                DapTaskConfig {
                    version,
                    leader_url: cmd.leader,
                    helper_url: cmd.helper,
                    time_precision: cmd.time_precision,
                    expiration: cmd.task_expiration,
                    min_batch_size: cmd.min_batch_size,
                    query,
                    vdaf,
                    dp_config,
                    vdaf_verify_key,
                    collector_hpke_config,
                    taskprov: false,
                },
            )
            .await?
            .is_some()
        {
            Err(int_err(format!(
                "command failed: config already exists for the given task ({})",
                cmd.task_id
            )))
        } else {
            Ok(())
        }
    }

    pub(crate) async fn internal_add_hpke_config(
        &self,
        version: DapVersion,
        new_receiver: HpkeReceiverConfig,
    ) -> Result<()> {
        let mut config_list = self
            .get_hpke_receiver_config(version, |config_list| Some(config_list.clone()))
            .await?
            .unwrap_or_default();

        if config_list
            .iter()
            .any(|receiver| new_receiver.config.id == receiver.config.id)
        {
            return Err(int_err(format!(
                "receiver config with id {} already exists",
                new_receiver.config.id
            )));
        }

        config_list.push(new_receiver);

        self.kv()?
            .put(
                &format!("{KV_KEY_PREFIX_HPKE_RECEIVER_CONFIG_SET}/{version}"),
                config_list.clone(),
            )?
            .execute()
            .await?;

        self.isolate_state()
            .hpke_receiver_configs
            .write()
            .unwrap()
            .insert(version, config_list);
        Ok(())
    }

    pub(crate) fn extract_version_parameter(&self, req: &Request) -> Result<DapVersion> {
        let url = req.url()?;
        let path = url.path();
        let mut router: Router<bool> = Router::new();
        router.insert("/:version/*remaining", true).unwrap();
        let url_match = router.at(path).unwrap();
        let version = url_match
            .params
            .get("version")
            .ok_or_else(|| Error::RustError(format!("Failed to parse path: {path}")))?;
        Ok(DapVersion::from(version))
    }

    pub(crate) async fn worker_request_to_dap<D>(
        &self,
        mut req: Request,
        ctx: &RouteContext<D>,
    ) -> Result<DapRequest<DaphneWorkerAuth>> {
        let version = self.extract_version_parameter(&req)?;

        // Determine the authorization method used by the sender.
        let sender_auth = Some(DaphneWorkerAuth {
            bearer_token: req.headers().get("DAP-Auth-Token")?.map(BearerToken::from),

            // The runtime gives us a cf_tls_client_auth whether the communication was secured by
            // it or not, so if a certificate wasn't presented, treat it as if it weren't there.
            // Literal "1" indicates that a certificate was presented.
            cf_tls_client_auth: req
                .cf()
                .tls_client_auth()
                .filter(|auth| auth.cert_presented() == "1"),
        });

        let content_type = req.headers().get("Content-Type")?;
        let media_type = DapMediaType::from_str_for_version(version, content_type.as_deref());

        let payload = req.bytes().await?;

        let (task_id, resource) = match version {
            DapVersion::Draft02 => {
                // Parse the task ID from the front of the request payload and use it to look up the
                // expected bearer token.
                //
                // TODO(cjpatton) Add regression tests that ensure each protocol message is prefixed by the
                // task ID.
                //
                // TODO spec: Consider moving the task ID out of the payload. Right now we're parsing it
                // twice so that we have a reference to the task ID before parsing the entire message.
                let mut r = Cursor::new(payload.as_ref());
                (TaskId::decode(&mut r).ok(), DapResource::Undefined)
            }
            DapVersion::Draft05 => {
                let task_id = ctx.param("task_id").and_then(TaskId::try_from_base64url);
                let resource = match media_type {
                    DapMediaType::AggregationJobInitReq
                    | DapMediaType::AggregationJobContinueReq => {
                        if let Some(agg_job_id) = ctx
                            .param("agg_job_id")
                            .and_then(AggregationJobId::try_from_base64url)
                        {
                            DapResource::AggregationJob(agg_job_id)
                        } else {
                            // Missing or invalid agg job ID. This should be handled as a bad
                            // request (undefined resource) by the caller.
                            DapResource::Undefined
                        }
                    }
                    DapMediaType::CollectReq => {
                        if let Some(collect_job_id) = ctx
                            .param("collect_job_id")
                            .and_then(CollectionJobId::try_from_base64url)
                        {
                            DapResource::CollectionJob(collect_job_id)
                        } else {
                            // Missing or invalid agg job ID. This should be handled as a bad
                            // request (undefined resource) by the caller.
                            DapResource::Undefined
                        }
                    }
                    _ => DapResource::Undefined,
                };

                (task_id, resource)
            }
            DapVersion::Unknown => unreachable!("unhandled version {version:?}"),
        };

        Ok(DapRequest {
            version,
            task_id,
            resource,
            payload,
            url: req.url()?,
            media_type,
            sender_auth,
            taskprov: req.headers().get("dap-taskprov")?,
        })
    }

    pub(crate) fn least_valid_report_time(&self, now: u64) -> u64 {
        now.saturating_sub(self.config().global.report_storage_epoch_duration)
    }

    pub(crate) fn greatest_valid_report_time(&self, now: u64) -> u64 {
        now.saturating_add(self.config().global.report_storage_max_future_time_skew)
    }

    // Generic HTTP POST/PUT
    pub(crate) async fn send_http(
        &self,
        req: DapRequest<DaphneWorkerAuth>,
        is_put: bool,
    ) -> std::result::Result<DapResponse, DapError> {
        let (payload, url) = (req.payload, req.url);

        let mut headers = reqwest_wasm::header::HeaderMap::new();

        let content_type = req
            .media_type
            .as_str_for_version(req.version)
            .ok_or_else(|| {
                fatal_error!(
                    err = "failed to construct content-type",
                    ?req.media_type,
                    ?req.version,
                )
            })?;

        headers.insert(
            reqwest_wasm::header::CONTENT_TYPE,
            reqwest_wasm::header::HeaderValue::from_str(content_type)
                .map_err(|e| fatal_error!(err = e, "failed to construct content-type header"))?,
        );

        if let Some(bearer_token) = req.sender_auth.and_then(|auth| auth.bearer_token) {
            headers.insert(
                reqwest_wasm::header::HeaderName::from_static("dap-auth-token"),
                reqwest_wasm::header::HeaderValue::from_str(bearer_token.as_ref()).map_err(
                    |e| fatal_error!(err = e, "failed to construct dap-auth-token header"),
                )?,
            );
        }

        let client = &self.isolate_state().client;
        let reqwest_req = if is_put {
            client.put(url.as_str())
        } else {
            client.post(url.as_str())
        }
        .body(payload)
        .headers(headers);

        let start = Date::now().as_millis();
        let reqwest_resp = reqwest_req
            .send()
            .await
            .map_err(|e| fatal_error!(err = e))?;
        let end = Date::now().as_millis();
        info!("request to {} completed in {}ms", url, end - start);
        let status = reqwest_resp.status();
        if status == 200 {
            // Translate the reqwest response into a Worker response.
            let content_type = reqwest_resp
                .headers()
                .get(reqwest_wasm::header::CONTENT_TYPE)
                .ok_or_else(|| fatal_error!(err = INT_ERR_PEER_RESP_MISSING_MEDIA_TYPE))?
                .to_str()
                .map_err(|e| fatal_error!(err = e))?;
            let media_type = DapMediaType::from_str_for_version(req.version, Some(content_type));

            let payload = reqwest_resp
                .bytes()
                .await
                .map_err(|e| fatal_error!(err = e))?
                .to_vec();

            Ok(DapResponse {
                version: req.version,
                payload,
                media_type,
            })
        } else {
            error!("{}: request failed: {:?}", url, reqwest_resp);
            if status == 400 {
                if let Some(content_type) = reqwest_resp
                    .headers()
                    .get(reqwest_wasm::header::CONTENT_TYPE)
                {
                    if content_type == "application/problem+json" {
                        error!(
                            "Problem details: {}",
                            reqwest_resp
                                .text()
                                .await
                                .map_err(|e| fatal_error!(err = e))?
                        );
                    }
                }
            }
            Err(fatal_error!(err = INT_ERR_PEER_ABORT))
        }
    }
}

/// KV pair
pub(crate) struct KvPair<'a, K: Clone, V> {
    key: Cow<'a, K>,
    value: V,
}

impl<K: Clone + Eq + std::hash::Hash, V> KvPair<'_, K, V> {
    pub(crate) fn key(&self) -> &K {
        self.key.as_ref()
    }

    pub(crate) fn value(&self) -> &V {
        &self.value
    }
}

pub(crate) type BearerTokenKvPair<'a> = KvPair<'a, TaskId, BearerToken>;

impl AsRef<BearerToken> for BearerTokenKvPair<'_> {
    fn as_ref(&self) -> &BearerToken {
        self.value()
    }
}

pub(crate) type DapTaskConfigKvPair<'a> = KvPair<'a, TaskId, DapTaskConfig>;

impl AsRef<DapTaskConfig> for DapTaskConfigKvPair<'_> {
    fn as_ref(&self) -> &DapTaskConfig {
        self.value()
    }
}

/// Deployment types for Daphne-Worker. This defines overrides used to control inter-Aggregator
/// communication.
#[derive(Debug, Default)]
pub(crate) enum DaphneWorkerDeployment {
    /// Daphne-Worker is running in a production environment. No behavior overrides are applied.
    #[default]
    Prod,
    /// Daphne-Worker is running in a development environment. Any durable objects that are created
    /// will be registered by the garbage collector so that they can be deleted manually using the
    /// internal test API.
    Dev,
}
