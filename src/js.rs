use std::cell::Cell;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rquickjs::function::{Async, This};
use rquickjs::{
    AsyncContext, AsyncRuntime, CString, Ctx, Exception, Function, Object, Promise, Value, qjs,
};
use tokio::sync::Semaphore;

use crate::decisions::{self, DecisionsError, Request};
use crate::diagnostic;
use crate::memory::LimitedAllocator;

const STACK_LIMIT: usize = 512 * 1024;
const SYSTEM1_ERROR: &str = "System1Error";
pub const MAX_DECISION_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_EXECUTE_RESULT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    pub memory_limit: usize,
}

#[derive(Debug)]
pub struct System1 {
    client: Option<decisions::Client>,
    permits: Semaphore,
    max_calls: NonZeroU32,
}

#[derive(Debug, thiserror::Error)]
pub enum JsError {
    #[error("{0}")]
    Exception(String),
    #[error("execution exceeded the {}ms deadline", .0.as_millis())]
    Timeout(Duration),
    #[error("execution exceeded the {0} byte memory limit")]
    OutOfMemory(usize),
    #[error("no value returned; use an explicit top-level `return`")]
    MissingReturn,
    #[error("result is not JSON-serializable: {0}")]
    UnsupportedResult(String),
    #[error("serialized execution result exceeds the {MAX_EXECUTE_RESULT_BYTES} byte limit")]
    ResultTooLarge,
    #[error("JavaScript engine error: {0}")]
    Engine(#[source] rquickjs::Error),
    #[error("result JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
enum DecideError {
    #[error("system1.decide() is unavailable because no [provider] is configured")]
    ProviderNotConfigured,
    #[error("system1.decide() may be called at most {0} times per execution")]
    CallLimitExceeded(NonZeroU32),
    #[error("decision input is not JSON-serializable: {0}")]
    Unserializable(String),
    #[error("serialized decision input exceeds the {MAX_DECISION_INPUT_BYTES} byte limit")]
    InputTooLarge,
    #[error("invalid decision input: {0}")]
    InvalidInput(#[source] serde_json::Error),
    #[error(transparent)]
    Decisions(#[from] DecisionsError),
    #[error("failed to encode the decision response: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("the inference request limiter is closed")]
    LimiterClosed,
}

impl JsError {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Exception(_) => "exception",
            Self::Timeout(_) => "timeout",
            Self::OutOfMemory(_) => "out_of_memory",
            Self::MissingReturn => "missing_return",
            Self::UnsupportedResult(_) => "unsupported_result",
            Self::ResultTooLarge => "result_too_large",
            Self::Engine(_) => "engine",
            Self::Json(_) => "json",
        }
    }
}

impl DecideError {
    fn kind(&self) -> &'static str {
        match self {
            Self::ProviderNotConfigured => "provider_not_configured",
            Self::CallLimitExceeded(_) => "call_limit_exceeded",
            Self::Unserializable(_) | Self::InputTooLarge | Self::InvalidInput(_) => {
                "invalid_input"
            }
            Self::Decisions(error) => error.kind(),
            Self::Encode(_) => "encode",
            Self::LimiterClosed => "limiter_closed",
        }
    }

    fn throw(&self, ctx: &Ctx<'_>) -> rquickjs::Error {
        let exception = match Exception::from_message(ctx.clone(), &diagnostic::message(self)) {
            Ok(exception) => exception,
            Err(error) => return error,
        };
        let object = exception.as_object();
        if let Err(error) = object
            .set("name", SYSTEM1_ERROR)
            .and_then(|()| object.set("kind", self.kind()))
        {
            return error;
        }
        ctx.throw(exception.into_value())
    }
}

impl System1 {
    #[must_use]
    pub fn new(
        client: Option<decisions::Client>,
        max_concurrent_requests: usize,
        max_calls: NonZeroU32,
    ) -> Self {
        Self {
            client,
            permits: Semaphore::new(max_concurrent_requests),
            max_calls,
        }
    }

    #[must_use]
    pub fn max_calls(&self) -> NonZeroU32 {
        self.max_calls
    }

    fn prepare<'js>(
        &self,
        ctx: &Ctx<'js>,
        input: Value<'js>,
        remaining: &Cell<u32>,
    ) -> Result<Request, DecideError> {
        if self.client.is_none() {
            return Err(DecideError::ProviderNotConfigured);
        }
        let json = match ctx.json_stringify(input) {
            Ok(Some(json)) => json.to_cstring(),
            Ok(None) => {
                return Err(DecideError::Unserializable(
                    "expected an object with `state` and `questions`".to_owned(),
                ));
            }
            Err(error) => Err(error),
        }
        .map_err(|error| DecideError::Unserializable(quickjs_message(ctx, &error)))?;
        if json.len() > MAX_DECISION_INPUT_BYTES {
            return Err(DecideError::InputTooLarge);
        }
        let request: Request =
            serde_json::from_slice(c_bytes(&json)).map_err(DecideError::InvalidInput)?;
        request.validate().map_err(DecisionsError::from)?;
        let left = remaining
            .get()
            .checked_sub(1)
            .ok_or(DecideError::CallLimitExceeded(self.max_calls))?;
        remaining.set(left);
        Ok(request)
    }

    async fn decide(&self, request: &Request) -> Result<String, DecideError> {
        let client = self
            .client
            .as_ref()
            .ok_or(DecideError::ProviderNotConfigured)?;
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| DecideError::LimiterClosed)?;
        let response = client.decide(request).await?;
        serde_json::to_string(&response).map_err(DecideError::Encode)
    }
}

struct Execution {
    deadline: Instant,
    limits: Limits,
    allocation_failed: Arc<AtomicBool>,
}

impl Execution {
    fn failure(&self) -> Option<JsError> {
        if Instant::now() >= self.deadline {
            Some(JsError::Timeout(self.limits.timeout))
        } else if self.allocation_failed.load(Ordering::Relaxed) {
            Some(JsError::OutOfMemory(self.limits.memory_limit))
        } else {
            None
        }
    }

    fn classify<'js>(
        &self,
        error: rquickjs::Error,
        ctx: Option<&Ctx<'js>>,
        stack: Option<&Function<'js>>,
    ) -> JsError {
        if let Some(failure) = self.failure() {
            return failure;
        }
        match (ctx, error) {
            (_, rquickjs::Error::Allocation) => JsError::OutOfMemory(self.limits.memory_limit),
            (Some(ctx), rquickjs::Error::Exception) => {
                let message = describe_exception(&ctx.catch(), stack);
                self.failure().unwrap_or(JsError::Exception(message))
            }
            (_, error) => JsError::Engine(error),
        }
    }
}

pub async fn run(
    code: &str,
    limits: Limits,
    system1: Arc<System1>,
    cancelled: impl Fn() -> bool + Send + 'static,
) -> Result<serde_json::Value, JsError> {
    let deadline = Instant::now() + limits.timeout;
    let (allocator, allocation_failed) = LimitedAllocator::new(limits.memory_limit);
    let execution = Arc::new(Execution {
        deadline,
        limits,
        allocation_failed,
    });
    let runtime = AsyncRuntime::new_with_alloc(allocator)
        .map_err(|error| execution.classify(error, None, None))?;
    runtime.set_max_stack_size(STACK_LIMIT).await;
    let interrupt = Arc::clone(&execution);
    runtime
        .set_interrupt_handler(Some(Box::new(move || {
            cancelled() || interrupt.failure().is_some()
        })))
        .await;
    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|error| execution.classify(error, None, None))?;
    let source = format!("(async () => {{\n{code}\n}})()");
    let mut work = std::pin::pin!(context.async_with(async |ctx| {
        let stack: Function = ctx
            .eval("Object.getOwnPropertyDescriptor(Error.prototype, 'stack').get")
            .map_err(|error| execution.classify(error, Some(&ctx), None))?;
        let classify = |error| execution.classify(error, Some(&ctx), Some(&stack));
        install(&ctx, system1, Arc::clone(&execution)).map_err(classify)?;
        let promise = ctx.eval::<Promise, _>(source).map_err(classify)?;
        let value = promise.into_future::<Value>().await.map_err(classify)?;
        to_json(&ctx, value, classify)
    }));
    let guarded = std::future::poll_fn(|cx| {
        if let Some(error) = execution.failure() {
            return std::task::Poll::Ready(Err(error));
        }
        let result = work.as_mut().poll(cx);
        execution
            .failure()
            .map_or(result, |error| std::task::Poll::Ready(Err(error)))
    });
    tokio::time::timeout_at(deadline.into(), guarded)
        .await
        .unwrap_or(Err(JsError::Timeout(limits.timeout)))
}

fn install<'js>(
    ctx: &Ctx<'js>,
    system1: Arc<System1>,
    execution: Arc<Execution>,
) -> rquickjs::Result<()> {
    let remaining = Cell::new(system1.max_calls.get());
    let decide = Function::new(
        ctx.clone(),
        Async(move |ctx: Ctx<'js>, input: Value<'js>| {
            let request = system1.prepare(&ctx, input, &remaining);
            let system1 = Arc::clone(&system1);
            let execution = Arc::clone(&execution);
            async move {
                if execution.failure().is_some() {
                    return Err(rquickjs::Error::Unknown);
                }
                let request = request.map_err(|error| error.throw(&ctx))?;
                let response = system1
                    .decide(&request)
                    .await
                    .map_err(|error| error.throw(&ctx))?;
                ctx.json_parse(response)
            }
        }),
    )?
    .with_name("decide")?;
    let namespace = Object::new(ctx.clone())?;
    namespace.set("decide", decide)?;
    ctx.globals().set("system1", namespace)
}

fn to_json<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    classify: impl Fn(rquickjs::Error) -> JsError,
) -> Result<serde_json::Value, JsError> {
    if value.is_undefined() {
        return Err(JsError::MissingReturn);
    }
    let type_name = value.type_name();
    match ctx.json_stringify(value) {
        Ok(Some(json)) => {
            let json = json.to_cstring().map_err(classify)?;
            if json.len() > MAX_EXECUTE_RESULT_BYTES {
                return Err(JsError::ResultTooLarge);
            }
            Ok(serde_json::from_slice(c_bytes(&json))?)
        }
        Ok(None) => Err(JsError::UnsupportedResult(type_name.to_owned())),
        Err(error) => match classify(error) {
            JsError::Exception(message) => Err(JsError::UnsupportedResult(message)),
            error => Err(error),
        },
    }
}

fn quickjs_message(ctx: &Ctx<'_>, error: &rquickjs::Error) -> String {
    if matches!(error, rquickjs::Error::Exception) || ctx.has_exception() {
        describe_exception(&ctx.catch(), None)
    } else {
        diagnostic::message(error)
    }
}

fn describe_exception<'js>(value: &Value<'js>, stack_getter: Option<&Function<'js>>) -> String {
    if value.is_error() {
        let name =
            error_property(value, qjs::JS_ATOM_name, 256).unwrap_or_else(|| "Error".to_owned());
        let message = error_property(value, qjs::JS_ATOM_message, 3 * 1024).unwrap_or_default();
        let stack = error_property(value, qjs::JS_ATOM_stack, 4 * 1024)
            .or_else(|| {
                let getter = stack_getter?;
                let Ok(stack) = getter.call::<_, Value>((This(value.clone()),)) else {
                    value.ctx().catch();
                    return None;
                };
                string_preview(&stack, 4 * 1024)
            })
            .unwrap_or_default();
        return if stack.is_empty() {
            format!("{name}: {message}")
        } else {
            format!("{name}: {message}\n{stack}")
        };
    }
    let text = if let Some(text) = string_preview(value, diagnostic::MAX_BYTES - 16) {
        text
    } else if let Some(number) = value.as_number() {
        number.to_string()
    } else if let Some(boolean) = value.as_bool() {
        boolean.to_string()
    } else {
        value.type_name().to_owned()
    };
    format!("Uncaught {text}")
}

fn error_property(value: &Value<'_>, key: qjs::JSAtom, budget: usize) -> Option<String> {
    let ctx = value.ctx();
    let raw = ctx.as_raw().as_ptr();
    let mut descriptor = std::mem::MaybeUninit::uninit();
    // SAFETY: The caller supplies a native Error from this context and a predefined atom.
    // A positive return initializes the descriptor and transfers its three value references.
    let property = unsafe {
        match qjs::JS_GetOwnProperty(raw, descriptor.as_mut_ptr(), value.as_raw(), key) {
            ..=-1 => {
                ctx.catch();
                return None;
            }
            0 => return None,
            _ => descriptor.assume_init(),
        }
    };
    // SAFETY: All three references are owned results from the same context.
    let property_value = unsafe {
        qjs::JS_FreeValue(raw, property.getter);
        qjs::JS_FreeValue(raw, property.setter);
        Value::from_raw(ctx.clone(), property.value)
    };
    string_preview(&property_value, budget)
}

fn string_preview(value: &Value<'_>, budget: usize) -> Option<String> {
    let Ok(text) = value.as_string()?.clone().to_cstring() else {
        value.ctx().catch();
        return None;
    };
    let bytes = c_bytes(&text);
    let end = bytes.len().min(budget + 1);
    Some(diagnostic::truncate(
        &String::from_utf8_lossy(&bytes[..end]),
        budget,
    ))
}

fn c_bytes<'a>(text: &'a CString<'_>) -> &'a [u8] {
    // SAFETY: CString owns the QuickJS buffer of len() bytes for the duration of this borrow.
    unsafe { std::slice::from_raw_parts(text.as_ptr().cast(), text.len()) }
}
