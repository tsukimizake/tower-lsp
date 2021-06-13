use std::collections::HashMap;
use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::future::BoxFuture;
use log::{error, info, warn};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

use super::{not_initialized_error, Error, Id, Request, Response, Result};
use crate::{ServerState, State};

type Handler<S> =
    Box<dyn Fn(Arc<S>, Arc<ServerState>, Request) -> BoxFuture<'static, Option<Response>> + Send>;

pub struct Router<S> {
    server: Arc<S>,
    methods: HashMap<&'static str, Handler<S>>,
    state: Arc<ServerState>,
}

impl<S: Send + Sync + 'static> Router<S> {
    pub(crate) fn new(server: S, state: Arc<ServerState>) -> Self {
        Router {
            server: Arc::new(server),
            methods: HashMap::new(),
            state,
        }
    }

    pub fn register_method<P, R, F>(&mut self, method_name: &'static str, handler: F)
    where
        P: FromParams,
        R: IntoResponse,
        F: for<'a> Method<'a, S, P, R> + Clone + Send + Sync + 'static,
    {
        self.methods.entry(method_name).or_insert_with(|| {
            Box::new(move |server, state, request| {
                let handler = handler.clone();
                Box::pin(async move {
                    let (method, id, params) = request.into_parts();

                    let params = match P::from_params(params) {
                        Ok(params) => params,
                        Err(err) => {
                            error!("invalid parameters for {:?} request", method);
                            return id.map(|id| Response::error(id, err));
                        }
                    };

                    match (&*method, state.get()) {
                        ("initialize", State::Uninitialized) => state.set(State::Initializing),
                        ("initialize", _) => {
                            warn!("received duplicate `initialize` request, ignoring");
                            return id.map(|id| Response::error(id, Error::invalid_request()));
                        }
                        ("shutdown", State::Initialized) => {
                            info!("shutdown request received, shutting down");
                            state.set(State::ShutDown);
                        }
                        ("exit", _) => {
                            info!("exit notification received, stopping");
                            state.set(State::Exited);
                            return None;
                        }
                        (_, State::Uninitialized) | (_, State::Initializing) => {
                            return id.map(|id| Response::error(id, not_initialized_error()))
                        }
                        (_, State::Initialized) => {}
                        _ => return id.map(|id| Response::error(id, Error::invalid_request())),
                    }

                    let response = handler.invoke(&*server, params).await.into_response(id);
                    match (&*method, &response) {
                        ("initialize", Some(res)) if res.is_ok() => state.set(State::Initialized),
                        ("initialize", _) => state.set(State::Uninitialized),
                        _ => {}
                    }

                    response
                })
            })
        });
    }

    pub fn call(&self, req: Request) -> Pin<Box<dyn Future<Output = Option<Response>> + Send>> {
        if let Some(handler) = self.methods.get(req.method()) {
            handler(self.server.clone(), self.state.clone(), req)
        } else {
            let (method, id, _) = req.into_parts();
            error!("method {:?} not found", method);
            Box::pin(async move {
                id.filter(|_| !method.starts_with("$/"))
                    .map(|id| Response::error(id, Error::method_not_found()))
            })
        }
    }
}

impl<S> Debug for Router<S> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct(stringify!(Router))
            .field("methods", &self.methods.keys())
            .finish()
    }
}

pub trait Method<'a, S, P, R> {
    type ResponseFuture: Future<Output = R> + Send;

    fn invoke(&self, server: &'a S, params: P) -> Self::ResponseFuture;
}

impl<'a, F, S, R, Fut> Method<'a, S, (), R> for F
where
    F: Fn(&'a S) -> Fut,
    S: 'a,
    Fut: Future<Output = R> + Send + 'a,
{
    type ResponseFuture = Fut;

    #[inline]
    fn invoke(&self, server: &'a S, _: ()) -> Self::ResponseFuture {
        self(server)
    }
}

impl<'a, F, S, P, R, Fut> Method<'a, S, (P,), R> for F
where
    F: Fn(&'a S, P) -> Fut,
    S: 'a,
    P: DeserializeOwned,
    Fut: Future<Output = R> + Send + 'a,
{
    type ResponseFuture = Fut;

    #[inline]
    fn invoke(&self, server: &'a S, params: (P,)) -> Self::ResponseFuture {
        self(server, params.0)
    }
}

pub trait FromParams: Send + Sized {
    fn from_params(params: Option<Value>) -> Result<Self>;
}

impl FromParams for () {
    fn from_params(params: Option<Value>) -> Result<Self> {
        if let Some(p) = params {
            Err(Error::invalid_params(format!("Unexpected params: {}", p)))
        } else {
            Ok(())
        }
    }
}

impl<P: DeserializeOwned + Send> FromParams for (P,) {
    fn from_params(params: Option<Value>) -> Result<Self> {
        if let Some(p) = params {
            serde_json::from_value(p)
                .map(|params| (params,))
                .map_err(|e| Error::invalid_params(e.to_string()))
        } else {
            Err(Error::invalid_params("Missing params field"))
        }
    }
}

pub trait IntoResponse {
    fn into_response(self, id: Option<Id>) -> Option<Response>;
}

impl IntoResponse for () {
    fn into_response(self, id: Option<Id>) -> Option<Response> {
        debug_assert!(id.is_none(), "Notifications never contain an `id` field");
        None
    }
}

impl<R: Serialize> IntoResponse for Result<R> {
    fn into_response(self, id: Option<Id>) -> Option<Response> {
        debug_assert!(id.is_some(), "Requests always contain an `id` field");
        if let Some(id) = id {
            let result = self.map(|r| serde_json::to_value(r).expect("must serialize into JSON"));
            Some(Response::from_parts(id, result))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const REQUEST_WITH_PARAMS: &'static str = "custom/requestWithParams";
    const REQUEST_WITHOUT_PARAMS: &'static str = "custom/requestWithoutParams";
    const NOTIF_WITH_PARAMS: &'static str = "custom/notifWithParams";
    const NOTIF_WITHOUT_PARAMS: &'static str = "custom/notifWithoutParams";

    struct Server;

    impl Server {
        async fn request_with_params(&self, _params: Vec<i32>) -> Result<String> {
            Ok("first".to_owned())
        }

        async fn request_without_params(&self) -> Result<String> {
            Ok("second".to_owned())
        }

        async fn notification_with_params(&self, _params: Vec<i32>) {}

        async fn notification_without_params(&self) {}
    }

    fn create_router() -> Router<Server> {
        let state = Arc::new(ServerState::new());
        state.set(State::Initialized);

        let mut router = Router::new(Server, state);
        router.register_method(REQUEST_WITH_PARAMS, Server::request_with_params);
        router.register_method(REQUEST_WITHOUT_PARAMS, Server::request_without_params);
        router.register_method(NOTIF_WITH_PARAMS, Server::notification_with_params);
        router.register_method(NOTIF_WITHOUT_PARAMS, Server::notification_without_params);
        router
    }

    fn create_request(method_name: &str, is_notification: bool, has_params: bool) -> Request {
        let value = match (is_notification, has_params) {
            (true, true) => json!({"jsonrpc":"2.0","method":method_name,"params":[]}),
            (true, false) => json!({"jsonrpc":"2.0","method":method_name}),
            (false, true) => json!({"jsonrpc":"2.0","method":method_name,"params":[],"id":1}),
            (false, false) => json!({"jsonrpc":"2.0","method":method_name,"id":1}),
        };

        serde_json::from_value(value).expect("Unable to create `Request` from `serde_json::Value`")
    }

    #[tokio::test]
    async fn dispatches_request_with_params() {
        let router = create_router();
        let response = router
            .call(create_request(REQUEST_WITH_PARAMS, false, true))
            .await;

        assert_eq!(response, Some(Response::ok(Id::Number(1), json!("first"))));
    }

    #[tokio::test]
    async fn dispatches_request_without_params() {
        let router = create_router();
        let response = router
            .call(create_request(REQUEST_WITHOUT_PARAMS, false, false))
            .await;

        assert_eq!(response, Some(Response::ok(Id::Number(1), json!("second"))));
    }

    #[tokio::test]
    async fn dispatches_notification_with_params() {
        let router = create_router();
        let response = router
            .call(create_request(NOTIF_WITH_PARAMS, true, true))
            .await;

        assert_eq!(response, None);
    }

    #[tokio::test]
    async fn dispatches_notification_without_params() {
        let router = create_router();
        let response = router
            .call(create_request(NOTIF_WITHOUT_PARAMS, true, false))
            .await;

        assert_eq!(response, None);
    }

    #[tokio::test]
    async fn rejects_unregistered_request() {
        let router = create_router();
        let response = router
            .call(create_request("custom/nonexistent", false, false))
            .await;

        assert_eq!(
            response,
            Some(Response::error(Id::Number(1), Error::method_not_found()))
        );
    }
}
