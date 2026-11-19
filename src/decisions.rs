use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};

use crate::config::Provider;

const MAX_QUESTION_NAME_LEN: usize = 64;
const PROBABILITY_SUM_TOLERANCE: f64 = 0.02;
const MAX_ERROR_MESSAGE_CHARS: usize = 512;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MIN_CHOICES: usize = 2;
const MIN_SCORE_LEVELS: usize = 2;
const MAX_NESTING_DEPTH: usize = 64;
const REDACTED: &str = "<redacted>";

/// A Decisions API request: the `state` to judge and the named questions to answer about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub state: serde_json::Value,
    pub questions: BTreeMap<String, Question>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Question {
    /// Yes/no question answered with the probability of `true`.
    Noul {
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// Picks one of the named choices, mapping each name to its description.
    Choice {
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        criteria: BTreeMap<String, String>,
    },
    /// Places the state on an ordered scale, lowest level first.
    Score {
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        criteria: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoulCriteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: Option<String>,
    /// The exact model build that answered.
    pub model: String,
    pub provider: Option<String>,
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul(NoulAnswer),
    Choice(ChoiceAnswer),
    Score(ScoreAnswer),
    /// The provider declined to answer this question.
    Refusal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoulAnswer {
    /// Probability of `true`, from 0 to 1.
    pub noul: f64,
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChoiceAnswer {
    /// The winning choice name.
    pub choice: String,
    pub probabilities: Option<BTreeMap<String, f64>>,
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreAnswer {
    /// Probability-weighted position on the 0-indexed scale, from 0 to the number of levels minus 1.
    pub score: f64,
    /// Probability per 0-indexed level, keyed by the level index as a string.
    pub probabilities: Option<BTreeMap<String, f64>>,
    /// Criterion text per 0-indexed level, keyed by the level index as a string.
    pub legend: Option<BTreeMap<String, String>>,
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Amount `OpenRouter` charged for the request.
    pub cost: Option<f64>,
    pub input_tokens_details: Option<InputTokensDetails>,
    pub cost_details: Option<CostDetails>,
    pub is_byok: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputTokensDetails {
    pub cached_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostDetails {
    #[serde(rename = "upstream_inference_cost")]
    pub upstream: Option<f64>,
    #[serde(rename = "upstream_inference_prompt_cost")]
    pub upstream_prompt: Option<f64>,
    #[serde(rename = "upstream_inference_completions_cost")]
    pub upstream_completions: Option<f64>,
}

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum RequestError {
    #[error("at least one question is required")]
    NoQuestions,
    #[error(
        "question name {0:?} must be 1 to {MAX_QUESTION_NAME_LEN} ASCII letters, digits, underscores or hyphens"
    )]
    InvalidQuestionName(String),
    #[error("state must be a string, object or array")]
    InvalidState,
    #[error("state nests objects and arrays deeper than {MAX_NESTING_DEPTH} levels")]
    StateTooDeep,
    #[error("choice question {0:?} needs at least {MIN_CHOICES} choices")]
    TooFewChoices(String),
    #[error("score question {0:?} needs at least {MIN_SCORE_LEVELS} levels")]
    TooFewLevels(String),
    #[error("question {0:?} has empty instructions")]
    EmptyInstructions(String),
    #[error("question {question:?} has an empty description for criterion {criterion:?}")]
    EmptyCriterion { question: String, criterion: String },
}

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum ResponseError {
    #[error("model is empty")]
    EmptyModel,
    #[error("no answer for question {0:?}")]
    MissingAnswer(String),
    #[error("answer for unknown question {0:?}")]
    UnexpectedAnswer(String),
    #[error("question {question:?} expected a {expected} answer but got {actual}")]
    TypeMismatch {
        question: String,
        expected: &'static str,
        actual: &'static str,
    },
    #[error("question {question:?} has probability {value} outside 0 to 1")]
    ProbabilityOutOfRange { question: String, value: f64 },
    #[error("question {question:?} has probabilities summing to {sum}")]
    ProbabilitySum { question: String, sum: f64 },
    #[error("question {question:?} answered with unknown choice {choice:?}")]
    UnknownChoice { question: String, choice: String },
    #[error("question {question:?} has score {score} outside 0 to {max}")]
    ScoreOutOfRange {
        question: String,
        score: f64,
        max: usize,
    },
    #[error("question {question:?} refers to unknown level {level}")]
    UnknownLevel { question: String, level: String },
    #[error("usage cost {0} is not a finite non-negative number")]
    InvalidCost(f64),
}

#[derive(Debug, thiserror::Error)]
pub enum DecisionsError {
    #[error("failed to build HTTP client: {0}")]
    Build(#[source] reqwest::Error),
    #[error("provider.api_key is not a valid HTTP header value")]
    InvalidApiKey,
    #[error("invalid request: {0}")]
    InvalidRequest(#[from] RequestError),
    #[error("request timed out after {}ms", .0.as_millis())]
    Timeout(Duration),
    #[error("HTTP request failed: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("response body exceeds {MAX_RESPONSE_BYTES} bytes")]
    ResponseTooLarge,
    #[error("authentication failed with HTTP {status}: {message}")]
    Authentication { status: StatusCode, message: String },
    #[error("Decisions API returned HTTP {status}: {message}")]
    Http { status: StatusCode, message: String },
    #[error("response is not a valid Decisions response: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("invalid response: {0}")]
    InvalidResponse(#[from] ResponseError),
}

impl DecisionsError {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Build(_) | Self::InvalidApiKey => "configuration",
            Self::InvalidRequest(_) => "invalid_input",
            Self::Timeout(_) => "timeout",
            Self::Transport(_) => "transport",
            Self::Authentication { .. } => "authentication",
            Self::Http { .. } => "http",
            Self::ResponseTooLarge | Self::Decode(_) | Self::InvalidResponse(_) => {
                "invalid_response"
            }
        }
    }
}

/// Reusable Decisions API client bound to one endpoint, model and API key.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    endpoint: Url,
    model: String,
    authorization: HeaderValue,
    timeout: Duration,
}

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    state: &'a serde_json::Value,
    questions: &'a BTreeMap<String, Question>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    message: String,
}

impl Client {
    pub fn new(provider: &Provider, timeout: Duration) -> Result<Self, DecisionsError> {
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {}", provider.api_key.expose()))
                .map_err(|_| DecisionsError::InvalidApiKey)?;
        authorization.set_sensitive(true);
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(Policy::none())
            .build()
            .map_err(DecisionsError::Build)?;
        Ok(Self {
            http,
            endpoint: provider.endpoint.clone(),
            model: provider.model.clone(),
            authorization,
            timeout,
        })
    }

    pub async fn decide(&self, request: &Request) -> Result<Response, DecisionsError> {
        request.validate()?;
        let body = Body {
            model: &self.model,
            state: &request.state,
            questions: &request.questions,
        };
        let response = self
            .http
            .post(self.endpoint.clone())
            .header(AUTHORIZATION, &self.authorization)
            .json(&body)
            .send()
            .await
            .map_err(|error| self.transport(error))?;
        let status = response.status();
        let bytes = self.read_body(response).await?;
        if !status.is_success() {
            return Err(self.status_error(status, &bytes));
        }
        let response: Response = serde_json::from_slice(&bytes).map_err(DecisionsError::Decode)?;
        response.validate(request)?;
        Ok(response)
    }

    async fn read_body(&self, mut response: reqwest::Response) -> Result<Vec<u8>, DecisionsError> {
        let expected = match response.content_length().map(usize::try_from) {
            Some(Ok(length)) if length <= MAX_RESPONSE_BYTES => length,
            Some(_) => return Err(DecisionsError::ResponseTooLarge),
            None => 0,
        };
        let mut body = Vec::with_capacity(expected);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| self.transport(error))?
        {
            if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
                return Err(DecisionsError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    fn transport(&self, error: reqwest::Error) -> DecisionsError {
        if error.is_timeout() {
            DecisionsError::Timeout(self.timeout)
        } else {
            DecisionsError::Transport(error.without_url())
        }
    }

    fn status_error(&self, status: StatusCode, body: &[u8]) -> DecisionsError {
        let message = serde_json::from_slice::<ErrorBody>(body).map_or_else(
            |_| {
                status
                    .canonical_reason()
                    .unwrap_or("unknown error")
                    .to_owned()
            },
            |body| self.redact(&body.error.message),
        );
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                DecisionsError::Authentication { status, message }
            }
            _ => DecisionsError::Http { status, message },
        }
    }

    fn redact(&self, message: &str) -> String {
        let mut sanitized = match self
            .authorization
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|key| !key.is_empty())
        {
            Some(key) => message.replace(key, REDACTED),
            None => message.to_owned(),
        };
        if let Some((end, _)) = sanitized.char_indices().nth(MAX_ERROR_MESSAGE_CHARS) {
            sanitized.truncate(end);
        }
        sanitized
    }
}

impl Request {
    pub fn validate(&self) -> Result<(), RequestError> {
        if !matches!(
            self.state,
            serde_json::Value::String(_)
                | serde_json::Value::Object(_)
                | serde_json::Value::Array(_)
        ) {
            return Err(RequestError::InvalidState);
        }
        if exceeds_depth(&self.state, MAX_NESTING_DEPTH) {
            return Err(RequestError::StateTooDeep);
        }
        if self.questions.is_empty() {
            return Err(RequestError::NoQuestions);
        }
        for (name, question) in &self.questions {
            if !is_valid_question_name(name) {
                return Err(RequestError::InvalidQuestionName(name.clone()));
            }
            question.validate(name)?;
        }
        Ok(())
    }
}

impl Question {
    fn validate(&self, name: &str) -> Result<(), RequestError> {
        let (Self::Noul { instructions, .. }
        | Self::Choice { instructions, .. }
        | Self::Score { instructions, .. }) = self;
        if instructions.as_deref().is_some_and(is_blank) {
            return Err(RequestError::EmptyInstructions(name.to_owned()));
        }
        let empty = |criterion: &str| RequestError::EmptyCriterion {
            question: name.to_owned(),
            criterion: criterion.to_owned(),
        };
        match self {
            Self::Noul { criteria, .. } => {
                if let Some(criteria) = criteria {
                    if is_blank(&criteria.yes) {
                        return Err(empty("true"));
                    }
                    if is_blank(&criteria.no) {
                        return Err(empty("false"));
                    }
                }
            }
            Self::Choice { criteria, .. } => {
                if criteria.len() < MIN_CHOICES {
                    return Err(RequestError::TooFewChoices(name.to_owned()));
                }
                if let Some((choice, _)) = criteria
                    .iter()
                    .find(|(_, description)| is_blank(description))
                {
                    return Err(empty(choice));
                }
            }
            Self::Score { criteria, .. } => {
                if criteria.len() < MIN_SCORE_LEVELS {
                    return Err(RequestError::TooFewLevels(name.to_owned()));
                }
                if let Some(level) = criteria.iter().position(|level| is_blank(level)) {
                    return Err(empty(&level.to_string()));
                }
            }
        }
        Ok(())
    }
}

fn is_blank(text: &str) -> bool {
    text.trim().is_empty()
}

fn exceeds_depth(value: &serde_json::Value, remaining: usize) -> bool {
    let nested = |child: &serde_json::Value| exceeds_depth(child, remaining - 1);
    match value {
        serde_json::Value::Array(items) => remaining == 0 || items.iter().any(nested),
        serde_json::Value::Object(fields) => remaining == 0 || fields.values().any(nested),
        _ => false,
    }
}

fn is_valid_question_name(name: &str) -> bool {
    (1..=MAX_QUESTION_NAME_LEN).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

impl Question {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Noul { .. } => "noul",
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
        }
    }
}

impl Answer {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Noul(_) => "noul",
            Self::Choice(_) => "choice",
            Self::Score(_) => "score",
            Self::Refusal => "refusal",
        }
    }
}

impl Response {
    pub fn validate(&self, request: &Request) -> Result<(), ResponseError> {
        if self.model.trim().is_empty() {
            return Err(ResponseError::EmptyModel);
        }
        if let Some(cost) = self
            .usage
            .cost
            .filter(|cost| !cost.is_finite() || *cost < 0.0)
        {
            return Err(ResponseError::InvalidCost(cost));
        }
        if let Some(name) = self
            .answers
            .keys()
            .find(|name| !request.questions.contains_key(*name))
        {
            return Err(ResponseError::UnexpectedAnswer(name.clone()));
        }
        for (name, question) in &request.questions {
            let answer = self
                .answers
                .get(name)
                .ok_or_else(|| ResponseError::MissingAnswer(name.clone()))?;
            match (question, answer) {
                (_, Answer::Refusal) => {}
                (Question::Noul { .. }, Answer::Noul(answer)) => {
                    probability(name, answer.noul)?;
                    confidence(name, answer.confidence)?;
                }
                (Question::Choice { criteria, .. }, Answer::Choice(answer)) => {
                    validate_choice(name, criteria, answer)?;
                }
                (Question::Score { criteria, .. }, Answer::Score(answer)) => {
                    validate_score(name, criteria.len(), answer)?;
                }
                (question, answer) => {
                    return Err(ResponseError::TypeMismatch {
                        question: name.clone(),
                        expected: question.kind(),
                        actual: answer.kind(),
                    });
                }
            }
        }
        Ok(())
    }
}

fn validate_choice(
    name: &str,
    criteria: &BTreeMap<String, String>,
    answer: &ChoiceAnswer,
) -> Result<(), ResponseError> {
    let unknown = |choice: &String| ResponseError::UnknownChoice {
        question: name.to_owned(),
        choice: choice.clone(),
    };
    if !criteria.contains_key(&answer.choice) {
        return Err(unknown(&answer.choice));
    }
    confidence(name, answer.confidence)?;
    if let Some(probabilities) = &answer.probabilities {
        if let Some(choice) = probabilities
            .keys()
            .find(|key| !criteria.contains_key(*key))
        {
            return Err(unknown(choice));
        }
        distribution(name, probabilities.values().copied())?;
    }
    Ok(())
}

fn validate_score(name: &str, levels: usize, answer: &ScoreAnswer) -> Result<(), ResponseError> {
    let max = levels.saturating_sub(1);
    if !(0.0..=f64::from(u32::try_from(max).unwrap_or(u32::MAX))).contains(&answer.score) {
        return Err(ResponseError::ScoreOutOfRange {
            question: name.to_owned(),
            score: answer.score,
            max,
        });
    }
    confidence(name, answer.confidence)?;
    let mut keys = answer
        .probabilities
        .iter()
        .flat_map(BTreeMap::keys)
        .chain(answer.legend.iter().flat_map(BTreeMap::keys));
    if let Some(level) =
        keys.find(|level| level.parse::<usize>().map_or(true, |level| level >= levels))
    {
        return Err(ResponseError::UnknownLevel {
            question: name.to_owned(),
            level: level.clone(),
        });
    }
    if let Some(probabilities) = &answer.probabilities {
        distribution(name, probabilities.values().copied())?;
    }
    Ok(())
}

fn probability(name: &str, value: f64) -> Result<(), ResponseError> {
    if (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(ResponseError::ProbabilityOutOfRange {
            question: name.to_owned(),
            value,
        })
    }
}

fn confidence(name: &str, value: Option<f64>) -> Result<(), ResponseError> {
    value.map_or(Ok(()), |value| probability(name, value))
}

fn distribution(name: &str, values: impl Iterator<Item = f64>) -> Result<(), ResponseError> {
    let mut sum = 0.0;
    for value in values {
        probability(name, value)?;
        sum += value;
    }
    if (sum - 1.0).abs() <= PROBABILITY_SUM_TOLERANCE {
        Ok(())
    } else {
        Err(ResponseError::ProbabilitySum {
            question: name.to_owned(),
            sum,
        })
    }
}
