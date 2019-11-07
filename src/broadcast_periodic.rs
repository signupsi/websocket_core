use crate::actix::Actor as ActixActor;
use crate::actix::ActorContext;
use crate::actix::AsyncContext;
use crate::actix::Running;
use crate::actix::StreamHandler;
use crate::actix_web::middleware;
use crate::actix_web::web;
use crate::actix_web::web::Data as ActixData;
use crate::actix_web::web::Payload;
use crate::actix_web::App as ActixApp;
use crate::actix_web::Error as HttpError;
use crate::actix_web::HttpRequest;
use crate::actix_web::HttpResponse;
use crate::actix_web::HttpServer as ActixHttpServer;
use crate::actix_web_actors::ws::start as ws_start;
use crate::actix_web_actors::ws::Message as WsMessage;
use crate::actix_web_actors::ws::ProtocolError as WsProtocolError;
use crate::actix_web_actors::ws::WebsocketContext;
use crate::common_types::CommonResponse;
use crate::debug;
use crate::futures::future::ok;
use crate::futures::prelude::*;
use crate::info;
use crate::ACTOR_MAILBOX_CAPACITY;
use crate::NOTFOUND_MESSAGE;
use std::collections::HashMap;
use std::io::Result as IOResult;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

pub struct PeriodicWebsocketConfig {
    pub binding_url: String,
    pub binding_path: String,
    pub max_clients: usize,
    pub periodic_interval: Duration,
    pub rapid_request_limit: Duration,
    pub periodic_message_getter: Arc<&'static (dyn Fn() -> String + Sync + Send)>,
}

pub struct PeriodicWebsocketState {
    pub active_clients: AtomicUsize,
    pub rejection_counter: AtomicUsize,
    pub config: PeriodicWebsocketConfig,
}

pub(crate) struct PeriodicBroadcastActor {
    last_request_stopwatch: Instant,
    rapid_request_limit: Duration,
    periodic_interval: Duration,
    client_closed_callback: Box<dyn Fn()>,
    periodic_message_getter: Arc<&'static (dyn Fn() -> String + Sync + Send)>,
}

impl PeriodicWebsocketState {
    #[inline]
    pub fn new(config: PeriodicWebsocketConfig) -> Self {
        Self {
            active_clients: AtomicUsize::new(0),
            rejection_counter: AtomicUsize::new(0),
            config,
        }
    }
}

impl PeriodicBroadcastActor {
    #[inline]
    fn new(config: &'static PeriodicWebsocketConfig, client_closed_callback: Box<dyn Fn()>) -> Self {
        Self {
            last_request_stopwatch: Instant::now(),
            rapid_request_limit: config.rapid_request_limit,
            periodic_interval: config.periodic_interval,
            client_closed_callback,
            periodic_message_getter: config.periodic_message_getter.clone(),
        }
    }
}

impl ActixActor for PeriodicBroadcastActor {
    type Context = WebsocketContext<Self>;

    #[inline]
    fn started(&mut self, context: &mut Self::Context) {
        context.set_mailbox_capacity(ACTOR_MAILBOX_CAPACITY);
        self.start_periodic_broadcast(context);
    }

    #[inline]
    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        (*self.client_closed_callback)();
        Running::Stop
    }
}

impl StreamHandler<WsMessage, WsProtocolError> for PeriodicBroadcastActor {
    #[inline]
    fn handle(&mut self, payload: WsMessage, context: &mut Self::Context) {
        if self.last_request_stopwatch.elapsed() < self.rapid_request_limit {
            context.stop();
            return;
        }
        self.last_request_stopwatch = Instant::now();
        match payload {
            WsMessage::Close(_) => context.stop(),
            WsMessage::Ping(ping_payload) => context.pong(&ping_payload),
            WsMessage::Text(text) => {
                if text.len() < 4 {
                    return;
                }
                if let "ping" = &text.to_lowercase()[0..4] {
                    context.text("pong")
                }
            }
            _ => (),
        }
    }
}

impl PeriodicBroadcastActor {
    #[inline]
    fn start_periodic_broadcast(&self, context: &mut <Self as ActixActor>::Context) {
        let tick_handler = self.periodic_message_getter.clone();
        context.run_interval(self.periodic_interval, move |_, ctx| {
            ctx.text(tick_handler());
        });
    }
}

#[inline]
fn reject_unmapped_handler(
    shared_state: ActixData<Arc<&'static PeriodicWebsocketState>>,
) -> Box<dyn Future<Item = HttpResponse, Error = HttpError>> {
    shared_state.rejection_counter.fetch_add(1, Ordering::Relaxed);
    debug!(
        "Rejected counter increased to {}",
        shared_state.rejection_counter.load(Ordering::Relaxed)
    );
    let mut error = Vec::default();
    error.push(NOTFOUND_MESSAGE.to_owned());
    let response_data = CommonResponse {
        error,
        result: HashMap::new(),
    };
    Box::new(ok::<_, HttpError>(
        HttpResponse::NotFound().body(serde_json::to_string(&response_data).unwrap()),
    ))
}

#[inline]
fn ws_upgrader(
    shared_state: ActixData<Arc<&'static PeriodicWebsocketState>>,
    request: HttpRequest,
    stream: Payload,
) -> Result<HttpResponse, HttpError> {
    let ref_clone = shared_state.clone();
    let upgrade_result = ws_start(
        PeriodicBroadcastActor::new(
            &ref_clone.config,
            Box::new(move || {
                let active_clients = ref_clone.active_clients.fetch_sub(1, Ordering::Relaxed);
                info!(
                    "Client connection closed, current active client is {}",
                    active_clients - 1
                );
            }),
        ),
        &request,
        stream,
    );
    if upgrade_result.is_ok() {
        let active_clients = shared_state.active_clients.fetch_add(1, Ordering::Relaxed);
        info!(
            "Client connection successful, current active client is {}",
            active_clients + 1
        );
    }
    upgrade_result
}

#[inline]
pub fn run_periodic_websocket_service(state: Arc<&'static PeriodicWebsocketState>) -> IOResult<()> {
    let binding_url = state.config.binding_url.clone();
    let binding_path = state.config.binding_path.clone();
    let max_clients = state.config.max_clients;
    let shared_data = ActixData::new(state.clone());
    ActixHttpServer::new(move || {
        ActixApp::new()
            .register_data(shared_data.clone())
            .wrap(middleware::Logger::default())
            .service(web::resource(&binding_path).route(web::get().to(ws_upgrader)))
            .default_service(web::route().to_async(reject_unmapped_handler))
    })
    .maxconn(max_clients)
    .shutdown_timeout(1)
    .bind(binding_url)
    .unwrap()
    .run()
}