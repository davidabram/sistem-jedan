use std::cell::Cell;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rquickjs::function::Async;
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Coerced, Ctx, Exception, Function,
    Object, Promise, Value,
};
use tokio::sync::Semaphore;

use crate::decisions::{self, DecisionsError, Request};

pub const STACK_LIMIT: usize = 512 * 1024;
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
    #[error("the promise returned by the code never settled")]
    NeverSettled,
    #[error("no value returned; use an explicit top-level `return`")]
    MissingReturn,
    #[error("result is not JSON-serializable: {0}")]
    UnsupportedResult(String),
    #[error("serialized execution result exceeds the {MAX_EXECUTE_RESULT_BYTES} byte limit")]
    ResultTooLarge,
    #[error("JavaScript engine error: {0}")]
    Engine(#[from] rquickjs::Error),
    #[error("result JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum DecideError {
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
            Self::NeverSettled => "never_settled",
            Self::MissingReturn => "missing_return",
            Self::UnsupportedResult(_) => "unsupported_result",
            Self::ResultTooLarge => "result_too_large",
            Self::Engine(_) => "engine",
            Self::Json(_) => "json",
        }
    }
}

impl DecideError {
    #[must_use]
    pub fn kind(&self) -> &'static str {
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
        let exception = match Exception::from_message(ctx.clone(), &self.to_string()) {
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
        max_concurrent_requests: NonZeroU32,
        max_calls: NonZeroU32,
    ) -> Self {
        Self {
            client,
            permits: Semaphore::new(max_concurrent_requests.get() as usize),
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
        let json = match ctx.json_stringify(input).catch(ctx) {
            Ok(Some(json)) => json
                .to_cstring()
                .map_err(|error| DecideError::Unserializable(error.to_string()))?,
            Ok(None) => {
                return Err(DecideError::Unserializable(
                    "expected an object with `state` and `questions`".to_owned(),
                ));
            }
            Err(error) => return Err(DecideError::Unserializable(caught_message(&error))),
        };
        if json.len() > MAX_DECISION_INPUT_BYTES {
            return Err(DecideError::InputTooLarge);
        }
        let request: Request =
            serde_json::from_str(json.as_str()).map_err(DecideError::InvalidInput)?;
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

pub async fn run(
    code: &str,
    limits: Limits,
    system1: Arc<System1>,
) -> Result<serde_json::Value, JsError> {
    let deadline = Instant::now() + limits.timeout;
    let runtime = AsyncRuntime::new()?;
    runtime.set_memory_limit(limits.memory_limit).await;
    runtime.set_max_stack_size(STACK_LIMIT).await;
    runtime
        .set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)))
        .await;
    let context = AsyncContext::full(&runtime).await?;
    let source = format!("(async () => {{\n{code}\n}})()");
    let execution = context.async_with(async move |ctx| {
        install(&ctx, system1).map_err(|error| caught(&ctx, error, deadline, limits))?;
        let promise = ctx
            .eval::<Promise, _>(source)
            .map_err(|error| caught(&ctx, error, deadline, limits))?;
        let value = promise
            .into_future::<Value>()
            .await
            .map_err(|error| caught(&ctx, error, deadline, limits))?;
        to_json(&ctx, value, deadline, limits)
    });
    match tokio::time::timeout_at(deadline.into(), execution).await {
        Ok(result) => result,
        Err(_) if runtime.is_job_pending().await => Err(JsError::Timeout(limits.timeout)),
        Err(_) => Err(JsError::NeverSettled),
    }
}

fn install<'js>(ctx: &Ctx<'js>, system1: Arc<System1>) -> rquickjs::Result<()> {
    let remaining = Cell::new(system1.max_calls.get());
    let decide = Function::new(
        ctx.clone(),
        Async(move |ctx: Ctx<'js>, input: Value<'js>| {
            let request = system1.prepare(&ctx, input, &remaining);
            let system1 = Arc::clone(&system1);
            async move {
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
    deadline: Instant,
    limits: Limits,
) -> Result<serde_json::Value, JsError> {
    if value.is_undefined() {
        return Err(JsError::MissingReturn);
    }
    let type_name = value.type_name();
    match ctx.json_stringify(value) {
        Ok(Some(json)) => {
            let json = json.to_cstring()?;
            if json.len() > MAX_EXECUTE_RESULT_BYTES {
                return Err(JsError::ResultTooLarge);
            }
            Ok(serde_json::from_str(json.as_str())?)
        }
        Ok(None) => Err(JsError::UnsupportedResult(type_name.to_owned())),
        Err(error) => match caught(ctx, error, deadline, limits) {
            JsError::Exception(message) => Err(JsError::UnsupportedResult(message)),
            error => Err(error),
        },
    }
}

fn caught_message(error: &CaughtError<'_>) -> String {
    match error {
        CaughtError::Exception(exception) => exception.message().unwrap_or_default(),
        CaughtError::Value(value) => value
            .get::<Coerced<String>>()
            .map_or_else(|error| error.to_string(), |Coerced(message)| message),
        CaughtError::Error(error) => error.to_string(),
    }
}

fn caught(ctx: &Ctx<'_>, error: rquickjs::Error, deadline: Instant, limits: Limits) -> JsError {
    match error {
        rquickjs::Error::Exception => {}
        rquickjs::Error::Allocation => return JsError::OutOfMemory(limits.memory_limit),
        error => return JsError::Engine(error),
    }
    let thrown = ctx.catch();
    if Instant::now() >= deadline {
        return JsError::Timeout(limits.timeout);
    }
    let Some(exception) = thrown.as_exception() else {
        return match thrown.get::<Coerced<String>>() {
            Ok(Coerced(message)) => JsError::Exception(format!("Uncaught {message}")),
            Err(error) => JsError::Engine(error),
        };
    };
    let message = exception.message().unwrap_or_default();
    if message == "out of memory" {
        return JsError::OutOfMemory(limits.memory_limit);
    }
    let name = exception
        .get::<_, Coerced<String>>("name")
        .map_or_else(|_| "Error".to_owned(), |Coerced(name)| name);
    match exception.stack().filter(|stack| !stack.is_empty()) {
        Some(stack) => JsError::Exception(format!("{name}: {message}\n{stack}")),
        None => JsError::Exception(format!("{name}: {message}")),
    }
}
