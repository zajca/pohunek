//! Tokio bridge used by the Iced shell.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

use std::future::Future;
use std::sync::LazyLock;

use futures::channel::{mpsc, oneshot};
use futures::{SinkExt, Stream, StreamExt};
use pohunek_gui_core::{HostConfig, Message as CoreMessage};
use tokio::runtime::{Builder, Runtime};

static TOKIO: LazyLock<Runtime> = LazyLock::new(|| {
    Builder::new_multi_thread()
        .enable_all()
        .thread_name("pohunek-gui-tokio")
        .build()
        .expect("build pohunek-gui tokio runtime")
});

pub(crate) fn perform<F, T>(future: F) -> impl Future<Output = T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    TOKIO.spawn(async move {
        let output = future.await;
        let _ = sender.send(output);
    });
    async move { receiver.await.expect("tokio task completed") }
}

pub(crate) fn host_subscription(config: &HostConfig) -> impl Stream<Item = CoreMessage> {
    let (mut sender, receiver) = mpsc::channel(128);
    let config = config.clone();
    TOKIO.spawn(async move {
        let mut stream = Box::pin(pohunek_gui_core::host_subscription_stream(config));
        while let Some(message) = stream.next().await {
            if sender.send(message).await.is_err() {
                break;
            }
        }
    });
    receiver
}
