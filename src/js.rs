use std::time::{Duration, Instant};

use rquickjs::{Coerced, Context, Ctx, Promise, Runtime, Value};

pub const STACK_LIMIT: usize = 512 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    pub memory_limit: usize,
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
    #[error("JavaScript engine error: {0}")]
    Engine(#[from] rquickjs::Error),
    #[error("result JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
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
            Self::Engine(_) => "engine",
            Self::Json(_) => "json",
        }
    }
}

pub fn run(code: &str, limits: Limits) -> Result<serde_json::Value, JsError> {
    let deadline = Instant::now() + limits.timeout;
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(limits.memory_limit);
    runtime.set_max_stack_size(STACK_LIMIT);
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
    let context = Context::full(&runtime)?;
    let source = format!("(async () => {{\n{code}\n}})()");
    context.with(|ctx| {
        let promise = ctx
            .eval::<Promise, _>(source)
            .map_err(|error| caught(&ctx, error, deadline, limits))?;
        let value = settle(&ctx, &promise, deadline, limits)?;
        to_json(&ctx, value, deadline, limits)
    })
}

fn settle<'js>(
    ctx: &Ctx<'js>,
    promise: &Promise<'js>,
    deadline: Instant,
    limits: Limits,
) -> Result<Value<'js>, JsError> {
    loop {
        if let Some(result) = promise.result::<Value>() {
            return result.map_err(|error| caught(ctx, error, deadline, limits));
        }
        if Instant::now() >= deadline {
            return Err(JsError::Timeout(limits.timeout));
        }
        if !ctx.execute_pending_job() {
            return Err(JsError::NeverSettled);
        }
    }
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
        Ok(Some(json)) => Ok(serde_json::from_str(&json.to_string()?)?),
        Ok(None) => Err(JsError::UnsupportedResult(type_name.to_owned())),
        Err(error) => match caught(ctx, error, deadline, limits) {
            JsError::Exception(message) => Err(JsError::UnsupportedResult(message)),
            error => Err(error),
        },
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
