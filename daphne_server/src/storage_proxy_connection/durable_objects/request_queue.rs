use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

/// A Queue of requests that throttles the maximum number of requests a durable object can receive.
pub(crate) struct RequestQueue {
    submit_requests_channel: mpsc::Sender<DurableObjectRequest>,
}

struct DurableObjectRequest {
    response_channel: oneshot::Sender<reqwest::Result<reqwest::Response>>,
    request: reqwest::RequestBuilder,
}

async fn request_buffer_task(
    concurrent_request_limit: usize,
    request_channel: mpsc::Receiver<DurableObjectRequest>,
) {
    ReceiverStream::new(request_channel)
        .map(|do_request| async {
            let _ = do_request
                .response_channel
                .send(do_request.request.send().await);
        })
        .buffered(concurrent_request_limit)
        .for_each(|_| futures::future::ready(()))
        .await;
}

impl RequestQueue {
    /// Create a new [`RequestQueue`] with the fixed maximum number of concurrent requests.
    pub fn new(concurrent_request_limit: usize) -> Self {
        // the channel does not need to buffer, all senders effectively await
        // both the send to complete and the response to be delivered
        let (submit_requests_channel_tx, submit_requests_channel_rx) = mpsc::channel(1);

        tokio::spawn(request_buffer_task(
            concurrent_request_limit,
            submit_requests_channel_rx,
        ));
        Self {
            submit_requests_channel: submit_requests_channel_tx,
        }
    }

    /// Submit a request to the queue and await it's response.
    pub async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> reqwest::Result<reqwest::Response> {
        let (tx, rx) = oneshot::channel();
        self.submit_requests_channel
            .send(DurableObjectRequest {
                response_channel: tx,
                request,
            })
            .await
            .expect("the receiver side of the RequestQueue was closed unexpectedly");

        rx.await
            .expect("the sender side of the oneshot channel was closed unexpectedly")
    }
}
