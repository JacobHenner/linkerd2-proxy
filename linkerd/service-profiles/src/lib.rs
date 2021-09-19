#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

use futures::Stream;
use linkerd_addr::{Addr, NameAddr};
use linkerd_cache as cache;
use linkerd_error::Error;
use linkerd_proxy_api_resolve::Metadata;
use std::{
    fmt,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::sync::watch;
use tower::util::{Oneshot, ServiceExt};

mod client;
mod discover;
pub mod http;
mod proto;
pub mod split;

pub use self::{client::Client, discover::NewDiscover};

#[derive(Clone, Debug)]
pub struct Receiver {
    inner: tokio::sync::watch::Receiver<Profile>,
    _handle: Option<cache::Handle>,
}

#[derive(Debug)]
struct ReceiverStream {
    inner: tokio_stream::wrappers::WatchStream<Profile>,
    _handle: Option<cache::Handle>,
}

#[derive(Clone, Debug, Default)]
pub struct Profile {
    pub addr: Option<LogicalAddr>,
    pub http_routes: Vec<(self::http::RequestMatch, self::http::Route)>,
    pub targets: Vec<Target>,
    pub opaque_protocol: bool,
    pub endpoint: Option<(SocketAddr, Metadata)>,
}

/// A profile lookup target.
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct LookupAddr(pub Addr);

/// A bound logical service address
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct LogicalAddr(pub NameAddr);

#[derive(Clone)]
pub struct Target {
    pub addr: NameAddr,
    pub weight: u32,
}

#[derive(Debug, Error)]
pub enum DiscoveryRejected {
    #[error("discovery rejected by control plane: {0}")]
    Remote(
        #[from]
        #[source]
        tonic::Status,
    ),
    #[error("discovery rejected: {0}")]
    Message(&'static str),
}

/// Watches a destination's Profile.
pub trait GetProfile {
    type Error: Into<Error>;
    type Future: Future<Output = Result<Option<Receiver>, Self::Error>>;

    fn get_profile(&self, target: LookupAddr) -> Self::Future;
}

impl<S> GetProfile for S
where
    S: tower::Service<LookupAddr, Response = Option<Receiver>> + Clone,
    S::Error: Into<Error>,
{
    type Error = S::Error;
    type Future = Oneshot<S, LookupAddr>;

    #[inline]
    fn get_profile(&self, target: LookupAddr) -> Self::Future {
        self.clone().oneshot(target)
    }
}
// === impl Receiver ===

impl Receiver {
    fn new(inner: watch::Receiver<Profile>, handle: cache::Handle) -> Self {
        Self {
            inner,
            _handle: Some(handle),
        }
    }

    /// Creates a receiver suitable for tests.
    pub fn for_test(inner: watch::Receiver<Profile>) -> Self {
        Self {
            inner,
            _handle: None,
        }
    }
}

impl Receiver {
    pub fn logical_addr(&self) -> Option<LogicalAddr> {
        self.inner.borrow().addr.clone()
    }

    pub fn is_opaque_protocol(&self) -> bool {
        self.inner.borrow().opaque_protocol
    }

    pub fn endpoint(&self) -> Option<(SocketAddr, Metadata)> {
        self.inner.borrow().endpoint.clone()
    }

    fn targets(&self) -> Vec<Target> {
        self.inner.borrow().targets.clone()
    }
}

// === impl ReceiverStream ===

impl From<Receiver> for ReceiverStream {
    fn from(Receiver { inner, _handle }: Receiver) -> Self {
        let inner = tokio_stream::wrappers::WatchStream::new(inner);
        ReceiverStream { inner, _handle }
    }
}

impl Stream for ReceiverStream {
    type Item = Profile;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

// === impl LookupAddr ===

impl fmt::Display for LookupAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for LookupAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LookupAddr({})", self.0)
    }
}

impl FromStr for LookupAddr {
    type Err = <Addr as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Addr::from_str(s).map(LookupAddr)
    }
}

impl From<Addr> for LookupAddr {
    fn from(a: Addr) -> Self {
        Self(a)
    }
}

impl From<LookupAddr> for Addr {
    fn from(LookupAddr(addr): LookupAddr) -> Addr {
        addr
    }
}

// === impl LogicalAddr ===

impl fmt::Display for LogicalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for LogicalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LogicalAddr({})", self.0)
    }
}

impl FromStr for LogicalAddr {
    type Err = <NameAddr as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        NameAddr::from_str(s).map(LogicalAddr)
    }
}

impl From<NameAddr> for LogicalAddr {
    fn from(na: NameAddr) -> Self {
        Self(na)
    }
}

impl From<LogicalAddr> for NameAddr {
    fn from(LogicalAddr(na): LogicalAddr) -> NameAddr {
        na
    }
}

// === impl Target ===

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Target")
            .field("addr", &format_args!("{}", self.addr))
            .field("weight", &self.weight)
            .finish()
    }
}

// === impl DiscoveryRejected ===

impl DiscoveryRejected {
    pub fn new(message: &'static str) -> Self {
        Self::Message(message)
    }

    pub fn is_rejected(err: &(dyn std::error::Error + 'static)) -> bool {
        let mut current = Some(err);
        while let Some(err) = current {
            if err.is::<Self>() {
                return true;
            }

            if let Some(status) = err.downcast_ref::<tonic::Status>() {
                let code = status.code();
                return
                    // Address is not resolveable
                    code == tonic::Code::InvalidArgument
                    // Unexpected cluster state
                    || code == tonic::Code::FailedPrecondition;
            }

            current = err.source();
        }
        false
    }
}
