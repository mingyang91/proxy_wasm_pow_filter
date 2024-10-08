pub mod task {
    mod singlethread;
    pub(crate) use singlethread::*;
}
pub mod codec;
pub mod counter_bucket;
pub mod kv_store;
pub mod lock;
pub mod log_level;
pub mod promise;
pub mod queue;
pub mod response;
pub mod timeout;

use std::{future::Future, rc::Rc, time::Duration};

use lock::{wake_tasks, QueueId};
use promise::{Promise, PENDINGS};
use proxy_wasm::{
    hostcalls,
    traits::{Context, HttpContext, RootContext},
    types::{Action, Status},
};
use response::Response;

/// Runs a Rust `Future` on the current thread.
///
/// The `future` must be `'static` because it will be scheduled
/// to run in the background and cannot contain any stack references.
///
/// The `future` will always be run on the next microtask tick even if it
/// immediately returns `Poll::Ready`.
///
/// # Panics
///
/// This function has the same panic behavior as `future_to_promise`.
#[inline]
pub fn spawn_local<F>(future: F)
where
    F: Future<Output = ()> + 'static,
{
    task::Task::spawn(Box::pin(future));
}

pub fn http_call(
    upstream: &str,
    headers: Vec<(&str, &str)>,
    body: Option<&[u8]>,
    trailers: Vec<(&str, &str)>,
    timeout: Duration,
) -> Result<Promise, Status> {
    let token = hostcalls::dispatch_http_call(upstream, headers, body, trailers, timeout)?;
    let promise = Promise::pending();
    PENDINGS.with(|pendings| pendings.insert(token, promise.clone()));
    Ok(promise)
}

pub trait Runtime: Context {
    type Hook: HttpHook + 'static;
    fn on_vm_start(&mut self, _vm_configuration_size: usize) -> bool {
        true
    }

    fn on_configure(&mut self, _configuration: Option<Vec<u8>>) -> bool {
        true
    }

    fn create_http_context(&self, _context_id: u32) -> Option<Self::Hook>;
}

pub struct RuntimeBox<R: Runtime> {
    inner: R,
}

impl<R: Runtime> RuntimeBox<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }
}

impl<R: Runtime> Context for RuntimeBox<R> {
    fn on_http_call_response(
        &mut self,
        token_id: u32,
        num_headers: usize,
        body_size: usize,
        _num_trailers: usize,
    ) {
        if let Some(promise) = PENDINGS.with(|pendings| pendings.remove(&token_id)) {
            if num_headers == 0 {
                promise.reject();
                return;
            }
            let headers = self.get_http_call_response_headers();
            let body = self.get_http_call_response_body(0, body_size);
            let trailers = self.get_http_call_response_trailers();
            let (code, _msg) = self.get_grpc_status();
            let response = Response {
                code,
                headers,
                body,
                trailers,
            };
            promise.resolve(response);
        }
    }
}

impl<R: Runtime> RootContext for RuntimeBox<R> {
    fn on_vm_start(&mut self, _vm_configuration_size: usize) -> bool {
        self.set_tick_period(Duration::from_millis(1));
        self.inner.on_vm_start(_vm_configuration_size)
    }

    fn on_configure(&mut self, _plugin_configuration_size: usize) -> bool {
        let content = self.get_plugin_configuration();
        self.inner.on_configure(content)
    }

    fn on_queue_ready(&mut self, queue_id: u32) {
        wake_tasks(QueueId(queue_id))
    }

    fn on_tick(&mut self) {
        queue::QUEUE.with(|queue| queue.on_tick())
    }

    fn create_http_context(&self, _context_id: u32) -> Option<Box<dyn HttpContext>> {
        let hook = self.inner.create_http_context(_context_id)?;
        Some(Box::new(HookHolder::<R::Hook>::new(_context_id, hook)))
    }

    fn get_type(&self) -> Option<proxy_wasm::types::ContextType> {
        Some(proxy_wasm::types::ContextType::HttpContext)
    }
}

#[derive(Clone, Copy)]
pub struct Ctx {
    id: u32,
}

impl Context for Ctx {}

impl HttpContext for Ctx {}

impl Ctx {
    pub fn new(id: u32) -> Self {
        Self { id }
    }

    pub fn get_client_address(&self) -> Result<Option<String>, Status> {
        hostcalls::set_effective_context(self.id)?;
        let Some(raw_property) = hostcalls::get_property(vec!["source", "address"])? else {
            return Ok(None);
        };
        let addr = String::from_utf8(raw_property).map_err(|e| {
            log::warn!("failed to parse client address: {}", e);
            Status::InternalFailure
        })?;
        Ok(Some(addr))
    }
    pub fn get_http_request_headers(&self) -> Result<Vec<(String, String)>, Status> {
        hostcalls::set_effective_context(self.id)?;
        Ok(HttpContext::get_http_request_headers(self))
    }

    pub fn get_http_request_header(&self, key: &str) -> Result<Option<String>, Status> {
        hostcalls::set_effective_context(self.id)?;
        Ok(HttpContext::get_http_request_header(self, key))
    }

    pub fn get_http_request_trailers(&self) -> Result<Vec<(String, String)>, Status> {
        hostcalls::set_effective_context(self.id)?;
        Ok(HttpContext::get_http_request_trailers(self))
    }

    fn continue_request(&self) -> Result<(), Status> {
        hostcalls::set_effective_context(self.id)?;
        hostcalls::resume_http_request()
    }

    fn reject_request(
        &self,
        status: u32,
        headers: Vec<(&str, &str)>,
        body: Option<&[u8]>,
    ) -> Result<(), Status> {
        hostcalls::set_effective_context(self.id)?;
        hostcalls::send_http_response(status, headers, body)
    }

    pub fn get_http_request_path(&self) -> Result<String, Status> {
        self.get_http_request_header(":path")?
            .ok_or(Status::BadArgument)
    }
}

pub trait HttpHook {
    fn filter_name() -> Option<&'static str> {
        None
    }

    fn on_request_headers(
        &self,
        _num_headers: usize,
        _end_of_stream: bool,
    ) -> impl Future<Output = Result<(), impl Into<Response>>> + Send;
}

pub struct HookHolder<H: HttpHook + 'static> {
    context: Ctx,
    inner: Rc<H>,
}

impl<H: HttpHook> HookHolder<H> {
    pub fn new(context_id: u32, inner: H) -> Self {
        Self {
            context: Ctx::new(context_id),
            inner: Rc::new(inner),
        }
    }
}

impl<H: HttpHook> Context for HookHolder<H> {}

impl<H: HttpHook> HttpContext for HookHolder<H> {
    fn on_http_request_trailers(&mut self, _num_trailers: usize) -> Action {
        let all = self.get_http_request_trailers();
        log::info!("all trailers: {:?}", all);
        Action::Continue
    }
    fn on_http_request_headers(&mut self, _num_headers: usize, _end_of_stream: bool) -> Action {
        log::debug!("on_http_request_headers");
        let hook = self.inner.clone();
        let ctx = self.context;
        spawn_local(async move {
            let res = hook.on_request_headers(_num_headers, _end_of_stream).await;
            let ret = match res {
                Ok(()) => ctx.continue_request(),
                Err(resp) => {
                    let resp = resp.into();
                    let code = resp.code;
                    let headers: Vec<(&str, &str)> = resp
                        .headers
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect();
                    log::debug!("reject http request");
                    ctx.reject_request(code, headers, resp.body.as_deref())
                }
            };
            if let Err(e) = ret {
                log::warn!("failed to resume http request: {:?}", e);
            }
        });
        Action::Pause
    }

    fn on_http_response_headers(&mut self, _num_headers: usize, _end_of_stream: bool) -> Action {
        log::debug!("on_http_response_headers");
        if let Some(name) = H::filter_name() {
            match self.get_http_response_header("X-Filter-Name") {
                Some(previous) => {
                    let p = format!("{}, {}", previous, name);
                    self.set_http_response_header("X-Filter-Name", Some(p.as_str()))
                }
                None => self.set_http_response_header("X-Filter-Name", Some(name)),
            }
        }
        Action::Continue
    }
}
