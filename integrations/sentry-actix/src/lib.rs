//! This crate adds a middleware for [`actix-web`](https://actix.rs/) that captures errors and
//! report them to `Sentry`.
//!
//! To use this middleware just configure Sentry and then add it to your actix web app as a
//! middleware.  Because actix is generally working with non sendable objects and highly concurrent
//! this middleware creates a new hub per request.  As a result many of the sentry integrations
//! such as breadcrumbs do not work unless you bind the actix hub.
//!
//! # Example
//!
//! ```
//! extern crate actix_web;
//! extern crate sentry;
//! extern crate sentry_actix;
//!
//! # fn main() {
//! use std::env;
//! use std::io;
//!
//! use actix_web::{server, App, Error, HttpRequest};
//! use sentry_actix::SentryMiddleware;
//!
//! fn failing(_req: &HttpRequest) -> Result<String, Error> {
//!     Err(io::Error::new(io::ErrorKind::Other, "An error happens here").into())
//! }
//!
//! fn main() {
//!     let _guard = sentry::init("https://public@sentry.io/1234");
//!     env::set_var("RUST_BACKTRACE", "1");
//!     sentry::integrations::panic::register_panic_handler();
//!
//!     server::new(|| {
//!         App::new()
//!             .middleware(SentryMiddleware::new())
//!             .resource("/", |r| r.f(failing))
//!     }).bind("127.0.0.1:3001")
//!         .unwrap()
//!         .run();
//! }
//! # }
//! ```
//!
//! # Reusing the Hub
//!
//! If you use this integration the `Hub::current()` returned hub is typically the wrong one.
//! To get the request specific one you need to use the `ActixWebHubExt` trait:
//!
//! ```
//! # extern crate sentry;
//! # extern crate sentry_actix;
//! # extern crate actix_web;
//! # fn test(req: &actix_web::HttpRequest) {
//! use sentry::{Hub, Level};
//! use sentry_actix::ActixWebHubExt;
//!
//! let hub = Hub::from_request(req);
//! hub.capture_message("Something is not well", Level::Warning);
//! # }
//! ```
//!
//! The hub can also be made current:
//!
//! ```
//! # extern crate sentry;
//! # extern crate sentry_actix;
//! # extern crate actix_web;
//! # fn test(req: &actix_web::HttpRequest) {
//! use sentry::{Hub, Level};
//! use sentry_actix::ActixWebHubExt;
//!
//! let hub = Hub::from_request(req);
//! Hub::run(hub, || {
//!     sentry::capture_message("Something is not well", Level::Warning);
//! });
//! # }
//! ```

use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use actix_service::{Service, Transform};
use actix_web::{
    dev::{ServiceRequest, ServiceResponse},
    Error, HttpMessage, HttpRequest,
    http::header::HeaderName,
};
use failure::Fail;
use futures::{future::{self, FutureResult}, Future, Poll};
use sentry::{
    integrations::failure::exception_from_single_fail,
    internals::{ScopeGuard, Uuid},
    protocol::{ClientSdkPackage, Event, Level},
    Hub,
};

/// A helper construct that can be used to reconfigure and build the middleware.
pub struct SentryHandlerBuilder {
    middleware: SentryHandler,
}

impl SentryHandlerBuilder {
    /// Finishes the building and returns a middleware
    pub fn finish(self) -> SentryHandler {
        self.middleware
    }

    /// Reconfigures the middleware so that it uses a specific hub instead of the default one.
    pub fn with_hub(mut self, hub: Arc<Hub>) -> Self {
        self.middleware.hub = Some(hub);
        self
    }

    /// Reconfigures the middleware so that it uses a specific hub instead of the default one.
    pub fn with_default_hub(mut self) -> Self {
        self.middleware.hub = None;
        self
    }

    /// If configured the sentry id is attached to a X-Sentry-Event header.
    pub fn emit_header(mut self, val: bool) -> Self {
        self.middleware.header = if val { Some(HeaderName::from_static("x-sentry-event")) } else { None };
        self
    }

    /// Enables or disables error reporting.
    ///
    /// The default is to report all errors.
    pub fn capture_server_errors(mut self, val: bool) -> Self {
        self.middleware.capture_server_errors = val;
        self
    }
}

/// Reports certain failures to sentry.
#[derive(Clone)]
pub struct SentryHandler {
    hub: Option<Arc<Hub>>,
    header: Option<HeaderName>,
    capture_server_errors: bool,
}

struct HubWrapper {
    hub: Arc<Hub>,
    root_scope: RefCell<Option<ScopeGuard>>,
}

impl SentryHandler {
    /// Creates a new sentry middleware.
    pub fn new() -> Self {
        Self {
            hub: None,
            header: None,
            capture_server_errors: true,
        }
    }

    /// Creates a new middleware builder.
    pub fn builder() -> SentryHandlerBuilder {
        Self::new().into_builder()
    }

    /// Converts the middleware into a builder.
    pub fn into_builder(self) -> SentryHandlerBuilder {
        SentryHandlerBuilder { middleware: self }
    }

    fn new_hub(&self) -> Arc<Hub> {
        Arc::new(Hub::new_from_top(Hub::main()))
    }
}

impl Default for SentryHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl<S, B> Transform<S> for SentryHandler
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = SentryMiddleware<S>;
    type Future = FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        future::ok(SentryMiddleware {
            service,
            inner: self.clone(),
        })
    }
}

/// The pieces of data from an HttpRequest we actually need
struct RequestBits {
    conn_info: actix_web::dev::ConnectionInfo,
    uri: actix_web::http::uri::Uri,
    method: actix_web::http::Method,
    headers: actix_web::http::header::HeaderMap,
}

impl From<&ServiceRequest> for RequestBits {
    fn from(req: &ServiceRequest) -> Self {
        // The HttpRequest inside the ServiceRequest has an Rc<RequestHead>
        // of which contains the headermap, so cloning is just bumping the
        // ref count, but that Rc isn't exposed anywhere, and headermap is
        // not itself clonable, so for now, just manually copy the headermap
        // until the API makes this better...someday
        let headers = {
            let headers = req.headers();
            let mut hm = actix_web::http::header::HeaderMap::with_capacity(headers.len());

            for (k, v) in headers {
                hm.append(k.clone(), v.clone());
            }

            hm
        };

        Self {
            conn_info: req.connection_info().clone(),
            uri: req.uri().clone(),
            method: *req.method(),
            headers,
        }
    }
}

fn extract_request(
    req: &RequestBits,
    with_pii: bool,
) -> (Option<String>, sentry::protocol::Request) {
    // There doesn't seem to be a way to retrieve the resource
    // definition that matches a request any longer, at least
    // from the public API

    // let resource = req.resource();
    // let transaction = if let Some(rdef) = resource.rdef() {
    //     Some(rdef.pattern().to_string())
    // } else if resource.name() != "" {
    //     Some(resource.name().to_string())
    // } else {
    //     None
    // };
    let transaction = None;

    let mut sentry_req = sentry::protocol::Request {
        url: format!(
            "{}://{}{}",
            req.conn_info.scheme(),
            req.conn_info.host(),
            req.uri,
        )
        .parse()
        .ok(),
        method: Some(req.method.to_string()),
        headers: req
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().into(), v.to_str().unwrap_or("").into()))
            .collect(),
        ..Default::default()
    };

    if with_pii {
        if let Some(remote) = req.conn_info.remote() {
            sentry_req.env.insert("REMOTE_ADDR".into(), remote.into());
        }
    };

    (transaction, sentry_req)
}

pub struct SentryMiddleware<S> {
    service: S,
    inner: SentryHandler,
}

impl<S, B> Service for SentryMiddleware<S>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, req: ServiceRequest) -> Self::Future {
        let hub = self.inner.new_hub();
        let outer_req = req;
        let req = RequestBits::from(&outer_req);
        let client = hub.client();

        let req = fragile::SemiSticky::new(req);
        let cached_data = Arc::new(Mutex::new(None));

        let root_scope = hub.push_scope();
        hub.configure_scope(move |scope| {
            scope.add_event_processor(Box::new(move |mut event| {
                let mut cached_data = cached_data.lock().unwrap();
                if cached_data.is_none() && req.is_valid() {
                    let with_pii = client
                        .as_ref()
                        .map_or(false, |x| x.options().send_default_pii);
                    *cached_data = Some(extract_request(&req.get(), with_pii));
                }

                if let Some((ref transaction, ref req)) = *cached_data {
                    if event.transaction.is_none() {
                        event.transaction = transaction.clone();
                    }
                    if event.request.is_none() {
                        event.request = Some(req.clone());
                    }
                }

                if let Some(sdk) = event.sdk.take() {
                    let mut sdk = sdk.into_owned();
                    sdk.packages.push(ClientSdkPackage {
                        name: "sentry-actix".into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                    });
                    event.sdk = Some(Cow::Owned(sdk));
                }

                Some(event)
            }));
        });

        outer_req.extensions_mut().insert(HubWrapper {
            hub,
            root_scope: RefCell::new(Some(root_scope)),
        });

        let inner = self.inner.clone();

        Box::new(self.service.call(outer_req).and_then(move |resp| {
            if inner.capture_server_errors && resp.status().is_server_error() {
                let event_id = if let Some(error) = resp.response().error() {
                    Some(Hub::from_request(resp.request()).capture_actix_error(error))
                } else {
                    None
                };
                match event_id {
                    Some(event_id) => {
                        if let Some(header) = inner.header {
                            resp.headers_mut().insert(
                                header,
                                event_id.to_simple_ref().to_string().parse().unwrap(),
                            );
                        }
                    }
                    _ => {}
                }
            }

            // if we make it to the end of the request we want to first drop the root
            // scope before we drop the entire hub.  This will first drop the closures
            // on the scope which in turn will release the circular dependency we have
            // with the hub via the request.
            if let Some(hub_wrapper) = resp.request().extensions().get::<HubWrapper>() {
                if let Ok(mut guard) = hub_wrapper.root_scope.try_borrow_mut() {
                    guard.take();
                }
            }

            resp
        }))
    }
}

/// Utility function that takes an actix error and reports it to the default hub.
///
/// This is typically not very useful since the actix hub is likely never bound as the
/// default hub.  It's generally recommended to use the `ActixWebHubExt` trait's
/// extension method on the hub instead.
pub fn capture_actix_error(err: &Error) -> Uuid {
    Hub::with_active(|hub| hub.capture_actix_error(err))
}

/// Hub extensions for actix.
pub trait ActixWebHubExt {
    /// Returns the hub from a given http request.
    ///
    /// This requires that the `SentryMiddleware` middleware has been enabled or the
    /// call will panic.
    fn from_request(req: &HttpRequest) -> Arc<Hub>;
    /// Captures an actix error on the given hub.
    fn capture_actix_error(&self, err: &Error) -> Uuid;
}

impl ActixWebHubExt for Hub {
    fn from_request(req: &HttpRequest) -> Arc<Hub> {
        req.extensions()
            .get::<HubWrapper>()
            .expect("SentryMiddleware middleware was not registered")
            .hub
            .clone()
    }

    fn capture_actix_error(&self, err: &Error) -> Uuid {
        let mut exceptions = vec![];
        let mut ptr: Option<&dyn Fail> = Some(err.as_fail());
        let mut idx = 0;
        while let Some(fail) = ptr {
            // Check whether the failure::Fail held by err is a failure::Error wrapped in Compat
            // If that's the case, we should be logging that error and its fail instead of the wrapper's construction in actix_web
            // This wouldn't be necessary if failure::Compat<failure::Error>'s Fail::backtrace() impl was not "|| None",
            // that is however impossible to do as of now because it conflicts with the generic implementation of Fail also provided in failure.
            // Waiting for update that allows overlap, (https://github.com/rust-lang/rfcs/issues/1053), but chances are by then failure/std::error will be refactored anyway
            let compat: Option<&failure::Compat<failure::Error>> = fail.downcast_ref();
            let failure_err = compat.map(failure::Compat::get_ref);
            let fail = failure_err.map_or(fail, |x| x.as_fail());
            exceptions.push(exception_from_single_fail(
                fail,
                if idx == 0 {
                    Some(failure_err.map_or_else(|| err.backtrace(), |err| err.backtrace()))
                } else {
                    fail.backtrace()
                },
            ));
            ptr = fail.cause();
            idx += 1;
        }
        exceptions.reverse();
        self.capture_event(Event {
            exception: exceptions.into(),
            level: Level::Error,
            ..Default::default()
        })
    }
}
