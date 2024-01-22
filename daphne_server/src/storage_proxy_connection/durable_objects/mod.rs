mod request_queue;

use daphne_service_utils::durable_requests::{
    bindings::DurableMethod, DurableRequest, ObjectIdFrom, DO_PATH_PREFIX,
};
use serde::{de::DeserializeOwned, Serialize};
use std::fmt::Debug;
use url::Url;

pub(crate) use request_queue::RequestQueue;

#[derive(Clone, Copy)]
pub(crate) struct Do<'h> {
    url: &'h Url,
    http: &'h reqwest::Client,
    retry: bool,
    request_queue: &'h RequestQueue,
}

impl<'h> Do<'h> {
    pub fn new(url: &'h Url, client: &'h reqwest::Client, request_queue: &'h RequestQueue) -> Self {
        Self {
            url,
            http: client,
            retry: false,
            request_queue,
        }
    }

    pub fn with_retry(self) -> Self {
        Self {
            retry: true,
            ..self
        }
    }
}

pub struct RequestBuilder<'d, B: DurableMethod, P: AsRef<[u8]>> {
    durable: &'d Do<'d>,
    path: B,
    request: DurableRequest<P>,
}

impl<'d, B: DurableMethod + Debug, P: AsRef<[u8]>> RequestBuilder<'d, B, P> {
    pub async fn send<R>(self) -> Result<R, super::Error>
    where
        R: DeserializeOwned,
    {
        tracing::debug!(
            obj = std::any::type_name::<B>().split("::").last().unwrap(),
            path = ?self.path,
            "requesting DO",
        );
        let url = self
            .durable
            .url
            .join(&format!("{DO_PATH_PREFIX}{}", self.path.to_uri()))
            .unwrap();
        let request_builder = self
            .durable
            .http
            .request(reqwest::Method::POST, url)
            .body(self.request.into_bytes());

        let resp = self.durable.request_queue.send(request_builder).await?;

        if resp.status().is_success() {
            Ok(resp.json().await?)
        } else {
            Err(super::Error::Http {
                status: super::status_reqwest_0_11_to_http_1_0(resp.status()),
                body: resp.text().await?,
            })
        }
    }
}

impl<'d, B: DurableMethod> RequestBuilder<'d, B, [u8; 0]> {
    pub fn encode_bincode<T: Serialize>(self, payload: T) -> RequestBuilder<'d, B, Vec<u8>> {
        self.with_body(bincode::serialize(&payload).unwrap())
    }

    pub fn with_body<T: AsRef<[u8]>>(self, payload: T) -> RequestBuilder<'d, B, T> {
        RequestBuilder {
            durable: self.durable,
            path: self.path,
            request: self.request.with_body(payload),
        }
    }
}

impl<'w> Do<'w> {
    pub fn request<B: DurableMethod + Copy>(
        &self,
        path: B,
        params: B::NameParameters<'_>,
    ) -> RequestBuilder<'_, B, [u8; 0]> {
        let (request, _) = DurableRequest::new(path, params);
        RequestBuilder {
            durable: self,
            path,
            request: if self.retry {
                request.with_retry()
            } else {
                request
            },
        }
    }

    pub fn request_with_id<B: DurableMethod + Copy>(
        &self,
        path: B,
        object_id: ObjectIdFrom,
    ) -> RequestBuilder<'_, B, [u8; 0]> {
        let (request, _) = DurableRequest::new_with_id(path, object_id);
        RequestBuilder {
            durable: self,
            path,
            request: if self.retry {
                request.with_retry()
            } else {
                request
            },
        }
    }
}
