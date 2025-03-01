//! Server implementation and builder.

use super::Error;
use crate::codegen::{BoxMessage, BoxMessageStream};
use async_stream::try_stream;
use futures::{future::poll_fn, select_biased, FutureExt, StreamExt};
use madsim::net::Endpoint;
use std::{
    collections::HashMap,
    convert::Infallible,
    future::{pending, Future},
    net::SocketAddr,
    time::Duration,
};
#[cfg(feature = "tls")]
use tonic::transport::ServerTlsConfig;
use tonic::{
    codegen::{http::uri::PathAndQuery, BoxFuture, Service},
    transport::NamedService,
};
use tower::{
    layer::util::{Identity, Stack},
    ServiceBuilder,
};

/// A default batteries included `transport` server.
#[derive(Clone, Debug)]
pub struct Server<L = Identity> {
    builder: ServiceBuilder<L>,
}

#[allow(clippy::derivable_impls)]
impl Default for Server {
    fn default() -> Self {
        Self {
            builder: Default::default(),
        }
    }
}

impl Server {
    /// Create a new server builder that can configure a [`Server`].
    pub fn builder() -> Self {
        Self::default()
    }
}

impl<L> Server<L> {
    /// Create a router with the `S` typed service as the first service.
    pub fn add_service<S>(&mut self, svc: S) -> Router<L>
    where
        S: Service<
                (SocketAddr, PathAndQuery, BoxMessageStream),
                Response = BoxMessageStream,
                Error = Infallible,
                Future = BoxFuture<BoxMessageStream, Infallible>,
            > + NamedService
            + Send
            + 'static,
        L: Clone,
    {
        let router = Router {
            server: self.clone(),
            services: Default::default(),
        };
        router.add_service(svc)
    }

    /// Set the Tower Layer all services will be wrapped in.
    pub fn layer<NewLayer>(self, new_layer: NewLayer) -> Server<Stack<NewLayer, L>> {
        log::warn!("layer is unimplemented and ignored");
        Server {
            builder: self.builder.layer(new_layer),
        }
    }

    /// Configure TLS for this server.
    #[cfg(feature = "tls")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
    pub fn tls_config(self, _tls_config: ServerTlsConfig) -> Result<Self, Error> {
        // ignore this setting
        Ok(self)
    }

    /// Set the concurrency limit applied to on requests inbound per connection.
    #[must_use]
    pub fn concurrency_limit_per_connection(self, _limit: usize) -> Self {
        // ignore this setting
        self
    }

    /// Set a timeout on for all request handlers.
    #[must_use]
    pub fn timeout(self, _timeout: Duration) -> Self {
        // ignore this setting
        self
    }

    /// Sets the `SETTINGS_INITIAL_WINDOW_SIZE` option for HTTP2 stream-level flow control.
    #[must_use]
    pub fn initial_stream_window_size(self, _sz: impl Into<Option<u32>>) -> Self {
        // ignore this setting
        self
    }

    /// Sets the max connection-level flow control for HTTP2
    #[must_use]
    pub fn initial_connection_window_size(self, _sz: impl Into<Option<u32>>) -> Self {
        // ignore this setting
        self
    }

    /// Sets the `SETTINGS_MAX_CONCURRENT_STREAMS` option for HTTP2 connections.
    #[must_use]
    pub fn max_concurrent_streams(self, _max: impl Into<Option<u32>>) -> Self {
        // ignore this setting
        self
    }

    /// Set whether HTTP2 Ping frames are enabled on accepted connections.
    #[must_use]
    pub fn http2_keepalive_interval(self, _http2_keepalive_interval: Option<Duration>) -> Self {
        // ignore this setting
        self
    }

    /// Sets a timeout for receiving an acknowledgement of the keepalive ping.
    #[must_use]
    pub fn http2_keepalive_timeout(self, _http2_keepalive_timeout: Option<Duration>) -> Self {
        // ignore this setting
        self
    }

    /// Set whether TCP keepalive messages are enabled on accepted connections.
    #[must_use]
    pub fn tcp_keepalive(self, _tcp_keepalive: Option<Duration>) -> Self {
        // ignore this setting
        self
    }

    /// Set the value of `TCP_NODELAY` option for accepted connections. Enabled by default.
    #[must_use]
    pub fn tcp_nodelay(self, _enabled: bool) -> Self {
        // ignore this setting
        self
    }

    /// Sets the maximum frame size to use for HTTP2.
    #[must_use]
    pub fn max_frame_size(self, _frame_size: impl Into<Option<u32>>) -> Self {
        // ignore this setting
        self
    }

    /// Allow this server to accept http1 requests.
    #[must_use]
    pub fn accept_http1(self, _accept_http1: bool) -> Self {
        // ignore this setting
        self
    }
}

/// A stack based `Service` router.
pub struct Router<L = Identity> {
    // TODO: support layers
    #[allow(dead_code)]
    server: Server<L>,

    #[allow(clippy::type_complexity)]
    services: HashMap<
        &'static str,
        Box<
            dyn Service<
                    (SocketAddr, PathAndQuery, BoxMessageStream),
                    Response = BoxMessageStream,
                    Error = Infallible,
                    Future = BoxFuture<BoxMessageStream, Infallible>,
                > + Send
                + 'static,
        >,
    >,
}

impl<L> Router<L> {
    /// Add a new service to this router.
    pub fn add_service<S>(mut self, svc: S) -> Self
    where
        S: Service<
                (SocketAddr, PathAndQuery, BoxMessageStream),
                Response = BoxMessageStream,
                Error = Infallible,
                Future = BoxFuture<BoxMessageStream, Infallible>,
            > + NamedService
            + Send
            + 'static,
    {
        self.services.insert(S::NAME, Box::new(svc));
        self
    }

    /// Consume this [`Server`] creating a future that will execute the server
    /// on default executor.
    pub async fn serve(self, addr: SocketAddr) -> Result<(), Error> {
        self.serve_with_shutdown(addr, pending::<()>()).await
    }

    /// Consume this [`Server`] creating a future that will execute the server
    /// on default executor. And shutdown when the provided signal is received.
    pub async fn serve_with_shutdown(
        mut self,
        addr: SocketAddr,
        signal: impl Future<Output = ()>,
    ) -> Result<(), Error> {
        let ep = Endpoint::bind(addr).await.map_err(Error::from_source)?;
        let mut signal = Box::pin(signal).fuse();
        loop {
            // receive a request
            let (tx, mut rx, from) = select_biased! {
                ret = ep.accept1().fuse() => ret.map_err(Error::from_source)?,
                _ = &mut signal => return Ok(()),
            };
            let msg = match rx.recv().await {
                Ok(msg) => msg,
                Err(_) => continue, // maybe handshake or error
            };
            let (path, msg) = *msg
                .downcast::<(PathAndQuery, BoxMessage)>()
                .expect("invalid type");
            log::debug!("request: {path} <- {from}");

            let requests: BoxMessageStream = if msg.downcast_ref::<()>().is_none() {
                // single request
                futures::stream::once(async move { Ok(msg) }).boxed()
            } else {
                // request stream
                try_stream! {
                    while let Ok(msg) = rx.recv().await {
                        yield msg;
                    }
                }
                .boxed()
            };

            // call the service in a new spawned task
            // TODO: handle error
            let svc_name = path.path().split('/').nth(1).unwrap();
            let svc = &mut self.services.get_mut(svc_name).unwrap();
            poll_fn(|cx| svc.poll_ready(cx)).await.unwrap();
            let rsp_future = svc.call((from, path, requests));
            madsim::task::spawn(async move {
                let mut stream = rsp_future.await.unwrap();
                // send the response
                while let Some(rsp) = stream.next().await {
                    // rsp: Result<BoxMessage, Status>
                    tx.send(Box::new(rsp)).await.unwrap();
                }
            });
        }
    }
}
