// Copyright (c) 2024 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

pub(crate) mod durable_objects;
pub(crate) mod kv;

use axum::http::{Method, StatusCode};

pub(crate) use durable_objects::Do;
pub(crate) use kv::Kv;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("network error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("http error. request returned status code {status} with the body {body}")]
    Http { status: StatusCode, body: String },
}

/// this is needed while [reqwest#2039](https://github.com/seanmonstar/reqwest/issues/2039) isn't
/// completed.
///
/// This is because axum is using http 1.0 and reqwest is still in http 0.2
pub fn method_http_1_0_to_reqwest_0_11(method: Method) -> reqwest::Method {
    match method {
        Method::GET => reqwest::Method::GET,
        Method::POST => reqwest::Method::POST,
        Method::PUT => reqwest::Method::PUT,
        Method::PATCH => reqwest::Method::PATCH,
        Method::HEAD => reqwest::Method::HEAD,
        Method::TRACE => reqwest::Method::TRACE,
        Method::OPTIONS => reqwest::Method::OPTIONS,
        Method::CONNECT => reqwest::Method::CONNECT,
        Method::DELETE => reqwest::Method::DELETE,
        _ => unreachable!(),
    }
}

/// this is needed while [reqwest#2039](https://github.com/seanmonstar/reqwest/issues/2039) isn't
/// completed.
///
/// This is because axum is using http 1.0 and reqwest is still in http 0.2
pub fn status_http_1_0_to_reqwest_0_11(status: StatusCode) -> reqwest::StatusCode {
    reqwest::StatusCode::from_u16(status.as_u16()).unwrap()
}

/// this is needed while [reqwest#2039](https://github.com/seanmonstar/reqwest/issues/2039) isn't
/// completed.
///
/// This is because axum is using http 1.0 and reqwest is still in http 0.2
pub fn status_reqwest_0_11_to_http_1_0(status: reqwest::StatusCode) -> StatusCode {
    StatusCode::from_u16(status.as_u16()).unwrap()
}
