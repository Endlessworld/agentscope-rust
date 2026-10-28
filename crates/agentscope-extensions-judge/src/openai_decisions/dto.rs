//! Typed Decisions request/result DTOs, mirroring
//! `io.agentscope.extensions.judge.openaidecisions.{DecisionInput, DecisionQuestion,
//! DecisionAnswer, DecisionRequest, DecisionResult, DecisionUsage}`.
//!
//! Java validates every constructor and throws `IllegalArgumentException`; the port returns
//! [`DecisionsError::InvalidArgument`] with the same messages. `DecisionInput` and
//! `DecisionRequest` keep their fields private so the invariants cannot be bypassed; the plain
//! value types expose public fields (construct them through the helpers when the invariant
//! matters — e.g. `DecisionQuestion::choice` enforces the 2..=255 option bound).

use std::collections::BTreeMap;

use super::error::DecisionsError;
use super::{CHOICE_OPTION_RANGE, MAX_IMAGES, MAX_QUESTIONS};

/// Message used whenever an image is neither a `data:image/...;base64,` URL nor an http(s) URL.
pub const IMAGE_URL_MESSAGE: &str = "image must be an image data URL or http(s) URL";

/// A choice value: Java `Object`, restricted to `String` or `Boolean`.
///
/// A string and a boolean with the same spelling stay distinct (`"true" != true`), exactly like
/// Java's `HashSet<Object>` uniqueness check in `DecisionQuestion.Choice`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DecisionValue {
    /// A textual option value.
    Text(String),
    /// A boolean option value.
    Bool(bool),
}

impl DecisionValue {
    /// Creates a textual value.
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    /// Creates a boolean value.
    pub fn boolean(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<&str> for DecisionValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for DecisionValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<bool> for DecisionValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

/// Text and/or images, sent as a string or a single user message (`DecisionInput`).
///
/// No image is fetched locally: data URLs and absolute http(s) URLs are accepted verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DecisionInput {
    text: Option<String>,
    image_urls: Vec<String>,
}

impl DecisionInput {
    /// Creates an input from optional text plus image URLs, applying Java's constructor checks.
    pub fn new(text: Option<String>, image_urls: Vec<String>) -> Result<Self, DecisionsError> {
        let input = Self { text, image_urls };
        input.validate()?;
        Ok(input)
    }

    /// Text-only input (`DecisionInput.text`).
    pub fn text(text: impl Into<String>) -> Result<Self, DecisionsError> {
        Self::new(Some(text.into()), Vec::new())
    }

    /// Image-only input (`DecisionInput.image`).
    pub fn image(image_url: impl Into<String>) -> Result<Self, DecisionsError> {
        Self::new(None, vec![image_url.into()])
    }

    /// Text plus images.
    pub fn with_images(
        text: impl Into<String>,
        image_urls: Vec<String>,
    ) -> Result<Self, DecisionsError> {
        Self::new(Some(text.into()), image_urls)
    }

    fn validate(&self) -> Result<(), DecisionsError> {
        if self.text.is_none() && self.image_urls.is_empty() {
            return Err(DecisionsError::invalid(
                "text or at least one image is required",
            ));
        }
        if self.image_urls.len() > MAX_IMAGES {
            return Err(DecisionsError::invalid(format!(
                "at most {MAX_IMAGES} images are allowed"
            )));
        }
        for image in &self.image_urls {
            validate_image_url(image)?;
        }
        Ok(())
    }

    /// The supplied text, if any.
    pub fn text_value(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// The supplied image URLs, in order.
    pub fn image_urls(&self) -> &[String] {
        &self.image_urls
    }

    /// Whether the input carries no text (image-only).
    pub fn is_image_only(&self) -> bool {
        self.text.is_none()
    }
}

/// Accepts a `data:image/...;base64,<payload>` URL or an absolute http(s) URL without
/// credentials (Java `DecisionInput`'s constructor loop).
fn validate_image_url(image: &str) -> Result<(), DecisionsError> {
    if image.starts_with("data:image/")
        && image.contains(";base64,")
        && !image.ends_with(";base64,")
    {
        return Ok(());
    }
    let url = reqwest::Url::parse(image).map_err(|_| DecisionsError::invalid(IMAGE_URL_MESSAGE))?;
    let scheme = url.scheme();
    let absolute_http = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    if !absolute_http
        || url.host_str().map(str::is_empty).unwrap_or(true)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(DecisionsError::invalid(IMAGE_URL_MESSAGE));
    }
    Ok(())
}

/// One legal choice value plus its optional description (`DecisionQuestion.Option`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionOption {
    /// The value the provider may select.
    pub value: DecisionValue,
    /// Optional human readable description.
    pub description: Option<String>,
}

impl DecisionOption {
    /// Creates an option.
    pub fn new(value: impl Into<DecisionValue>, description: Option<String>) -> Self {
        Self {
            value: value.into(),
            description,
        }
    }

    /// Creates a textual option without a description.
    pub fn text(value: impl Into<String>) -> Self {
        Self::new(DecisionValue::text(value), None)
    }

    /// Creates a boolean option without a description.
    pub fn boolean(value: bool) -> Self {
        Self::new(DecisionValue::boolean(value), None)
    }
}

/// One ordered rubric level (`DecisionQuestion.Level`), lowest to highest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionLevel {
    /// The level label the provider echoes back.
    pub label: String,
    /// Optional human readable description.
    pub description: Option<String>,
}

impl DecisionLevel {
    /// Creates a labelled level with an optional description.
    pub fn new(label: impl Into<String>, description: Option<String>) -> Self {
        Self {
            label: label.into(),
            description,
        }
    }

    /// Creates a labelled level without a description.
    pub fn labeled(label: impl Into<String>) -> Self {
        Self::new(label, None)
    }
}

/// A typed question about the shared input (`DecisionQuestion`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionQuestion {
    /// Estimates the probability that the instructions describe a true condition.
    Predicate {
        /// What to decide.
        instructions: String,
    },
    /// Selects one of 2..=255 distinct string or boolean values.
    Choice {
        /// What to decide.
        instructions: String,
        /// The legal options.
        choices: Vec<DecisionOption>,
    },
    /// Scores the input using ordered, zero-based levels.
    Score {
        /// What to decide.
        instructions: String,
        /// The rubric levels, lowest to highest.
        levels: Vec<DecisionLevel>,
    },
}

impl DecisionQuestion {
    /// Creates a predicate question.
    pub fn predicate(instructions: impl Into<String>) -> Self {
        Self::Predicate {
            instructions: instructions.into(),
        }
    }

    /// Creates a choice question, enforcing 2..=255 distinct option values.
    pub fn choice(
        instructions: impl Into<String>,
        choices: Vec<DecisionOption>,
    ) -> Result<Self, DecisionsError> {
        if !CHOICE_OPTION_RANGE.contains(&choices.len()) {
            return Err(DecisionsError::invalid(
                "choices must contain 2 to 255 options",
            ));
        }
        let mut seen: Vec<&DecisionValue> = Vec::with_capacity(choices.len());
        for option in &choices {
            if seen.contains(&&option.value) {
                return Err(DecisionsError::invalid("choice values must be unique"));
            }
            seen.push(&option.value);
        }
        Ok(Self::Choice {
            instructions: instructions.into(),
            choices,
        })
    }

    /// Creates a score question, enforcing at least one level.
    pub fn score(
        instructions: impl Into<String>,
        levels: Vec<DecisionLevel>,
    ) -> Result<Self, DecisionsError> {
        if levels.is_empty() {
            return Err(DecisionsError::invalid("levels must not be empty"));
        }
        Ok(Self::Score {
            instructions: instructions.into(),
            levels,
        })
    }

    /// The question's instructions.
    pub fn instructions(&self) -> &str {
        match self {
            DecisionQuestion::Predicate { instructions }
            | DecisionQuestion::Choice { instructions, .. }
            | DecisionQuestion::Score { instructions, .. } => instructions,
        }
    }

    /// The legal options, when this is a choice question.
    pub fn choices(&self) -> Option<&[DecisionOption]> {
        match self {
            DecisionQuestion::Choice { choices, .. } => Some(choices),
            _ => None,
        }
    }

    /// The rubric levels, when this is a score question.
    pub fn levels(&self) -> Option<&[DecisionLevel]> {
        match self {
            DecisionQuestion::Score { levels, .. } => Some(levels),
            _ => None,
        }
    }
}

/// One named question set over shared input (`DecisionRequest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRequest {
    input: DecisionInput,
    model: Option<String>,
    questions: BTreeMap<String, DecisionQuestion>,
    prompt_cache_key: Option<String>,
}

impl DecisionRequest {
    /// Creates a request, applying Java's record constructor checks.
    pub fn new(
        input: DecisionInput,
        model: Option<String>,
        questions: BTreeMap<String, DecisionQuestion>,
        prompt_cache_key: Option<String>,
    ) -> Result<Self, DecisionsError> {
        if questions.is_empty() || questions.len() > MAX_QUESTIONS {
            return Err(DecisionsError::invalid(format!(
                "questions must contain 1 to {MAX_QUESTIONS} entries"
            )));
        }
        if questions.keys().any(|name| name.trim().is_empty()) {
            return Err(DecisionsError::invalid("question names must not be blank"));
        }
        if model.as_ref().is_some_and(|model| model.trim().is_empty()) {
            return Err(DecisionsError::invalid("model must not be blank"));
        }
        if prompt_cache_key
            .as_ref()
            .is_some_and(|key| key.trim().is_empty())
        {
            return Err(DecisionsError::invalid("promptCacheKey must not be blank"));
        }
        Ok(Self {
            input,
            model,
            questions,
            prompt_cache_key,
        })
    }

    /// Starts building a request (`DecisionRequest.builder()`).
    pub fn builder() -> DecisionRequestBuilder {
        DecisionRequestBuilder::default()
    }

    /// The shared input.
    pub fn input(&self) -> &DecisionInput {
        &self.input
    }

    /// The model override, if any.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The questions, keyed by name.
    pub fn questions(&self) -> &BTreeMap<String, DecisionQuestion> {
        &self.questions
    }

    /// The prompt cache key, if any.
    pub fn prompt_cache_key(&self) -> Option<&str> {
        self.prompt_cache_key.as_deref()
    }
}

/// Incremental builder for [`DecisionRequest`] (`DecisionRequest.Builder`).
///
/// Java throws from `question(...)` on a duplicate name; the builder cannot, so `build()`
/// reports it with the same message. Every other check runs in [`DecisionRequest::new`].
#[derive(Debug, Clone, Default)]
pub struct DecisionRequestBuilder {
    input: Option<DecisionInput>,
    model: Option<String>,
    prompt_cache_key: Option<String>,
    questions: Vec<(String, DecisionQuestion)>,
}

impl DecisionRequestBuilder {
    /// Sets the input.
    pub fn input(mut self, input: DecisionInput) -> Self {
        self.input = Some(input);
        self
    }

    /// Sets a text-only input (`DecisionRequest.Builder.input(String)`).
    pub fn input_text(mut self, text: impl Into<String>) -> Self {
        self.input = Some(DecisionInput {
            text: Some(text.into()),
            image_urls: Vec::new(),
        });
        self
    }

    /// Sets the model override.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Sets the prompt cache key.
    pub fn prompt_cache_key(mut self, prompt_cache_key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(prompt_cache_key.into());
        self
    }

    /// Adds one question.
    pub fn question(mut self, name: impl Into<String>, question: DecisionQuestion) -> Self {
        self.questions.push((name.into(), question));
        self
    }

    /// Validates and builds the request.
    pub fn build(self) -> Result<DecisionRequest, DecisionsError> {
        let input = self
            .input
            .ok_or_else(|| DecisionsError::invalid("input must not be null"))?;
        let mut questions = BTreeMap::new();
        for (name, question) in self.questions {
            if questions.contains_key(&name) {
                return Err(DecisionsError::invalid("question names must be unique"));
            }
            questions.insert(name, question);
        }
        DecisionRequest::new(input, self.model, questions, self.prompt_cache_key)
    }
}

/// One entry of a choice answer's probability distribution.
#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceProbability {
    /// The option value.
    pub value: DecisionValue,
    /// Probability in `[0, 1]`.
    pub probability: f64,
}

/// One entry of a score answer's probability distribution.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreProbability {
    /// Zero-based level index.
    pub value: usize,
    /// The level label echoed by the provider.
    pub label: String,
    /// Probability in `[0, 1]`.
    pub probability: f64,
}

/// A validated answer, or an explicit refusal for one question (`DecisionAnswer`).
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionAnswer {
    /// Probability that the predicate holds.
    Predicate {
        /// Probability in `[0, 1]`.
        probability: f64,
    },
    /// The selected option plus its distribution.
    Choice {
        /// The selected option value (not necessarily the most probable one).
        choice: DecisionValue,
        /// Confidence in `[0, 1]`.
        confidence: f64,
        /// One entry per declared option.
        probabilities: Vec<ChoiceProbability>,
    },
    /// The weighted score plus its level distribution.
    Score {
        /// Probability-weighted score, within `[0, levels - 1]`.
        score: f64,
        /// Confidence in `[0, 1]`.
        confidence: f64,
        /// One entry per declared level.
        probabilities: Vec<ScoreProbability>,
    },
    /// The provider declined this question; other answers remain available.
    Refusal,
}

impl DecisionAnswer {
    /// Whether the provider refused this question.
    pub fn is_refusal(&self) -> bool {
        matches!(self, DecisionAnswer::Refusal)
    }
}

/// Provider token counts; missing counts stay `None`, never inferred as zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecisionUsage {
    /// Input tokens.
    pub input_tokens: Option<i64>,
    /// Output tokens.
    pub output_tokens: Option<i64>,
    /// Total tokens.
    pub total_tokens: Option<i64>,
    /// Cached input tokens.
    pub cached_tokens: Option<i64>,
    /// Cache-write input tokens.
    pub cache_write_tokens: Option<i64>,
    /// Reasoning output tokens.
    pub reasoning_tokens: Option<i64>,
}

/// Answers indexed by question name, plus the provider's model and optional usage
/// (`DecisionResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionResult {
    /// The model that answered.
    pub model: String,
    /// One answer per question name.
    pub answers: BTreeMap<String, DecisionAnswer>,
    /// Token usage, absent when the provider omitted it.
    pub usage: Option<DecisionUsage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn questions(entries: &[(&str, DecisionQuestion)]) -> BTreeMap<String, DecisionQuestion> {
        entries
            .iter()
            .map(|(name, question)| ((*name).to_string(), question.clone()))
            .collect()
    }

    fn predicate() -> DecisionQuestion {
        DecisionQuestion::predicate("Is the item damaged?")
    }

    fn choice_question() -> DecisionQuestion {
        DecisionQuestion::choice(
            "Select",
            vec![
                DecisionOption::text("true"),
                DecisionOption::new(true, Some("Needs repair".to_string())),
            ],
        )
        .unwrap()
    }

    fn score_question() -> DecisionQuestion {
        DecisionQuestion::score(
            "Rate",
            vec![
                DecisionLevel::labeled("minor"),
                DecisionLevel::labeled("blocked"),
            ],
        )
        .unwrap()
    }

    #[test]
    fn input_accepts_text_data_urls_and_http_urls() {
        let text = DecisionInput::text("The screen is broken").unwrap();
        assert_eq!(text.text_value(), Some("The screen is broken"));
        assert!(text.image_urls().is_empty());

        let image = DecisionInput::image("data:image/png;base64,aW1hZ2U=").unwrap();
        assert!(image.is_image_only());
        assert_eq!(image.text_value(), None);

        let both = DecisionInput::with_images(
            "Inspect",
            vec![
                "data:image/png;base64,aW1hZ2U=".to_string(),
                "https://images.example/product.png".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(both.image_urls().len(), 2);

        // 128 images is the documented maximum, and repeated URLs are allowed.
        let max = DecisionInput::new(
            None,
            std::iter::repeat_n("data:image/png;base64,aW1hZ2U=".to_string(), MAX_IMAGES).collect(),
        )
        .unwrap();
        assert_eq!(max.image_urls().len(), MAX_IMAGES);
    }

    #[test]
    fn input_rejects_missing_content_and_unsafe_images() {
        assert_eq!(
            DecisionInput::new(None, Vec::new()).unwrap_err().message(),
            "text or at least one image is required"
        );
        assert_eq!(
            DecisionInput::image("file:///private.png")
                .unwrap_err()
                .message(),
            IMAGE_URL_MESSAGE
        );
        assert_eq!(
            DecisionInput::image("https://user:secret@example.com/a.png")
                .unwrap_err()
                .message(),
            IMAGE_URL_MESSAGE
        );
        assert_eq!(
            DecisionInput::image("data:image/png;base64,")
                .unwrap_err()
                .message(),
            IMAGE_URL_MESSAGE,
            "an empty data URL payload falls through to the URL branch"
        );
        assert_eq!(
            DecisionInput::new(
                None,
                std::iter::repeat_n("https://x/a.png".to_string(), 129).collect()
            )
            .unwrap_err()
            .message(),
            "at most 128 images are allowed"
        );
    }

    #[test]
    fn choice_questions_enforce_bounds_and_unique_values() {
        assert_eq!(
            DecisionQuestion::choice("x", vec![DecisionOption::text("a")])
                .unwrap_err()
                .message(),
            "choices must contain 2 to 255 options"
        );
        assert_eq!(
            DecisionQuestion::choice(
                "x",
                vec![DecisionOption::text("a"), DecisionOption::text("a")]
            )
            .unwrap_err()
            .message(),
            "choice values must be unique"
        );

        // A string and a boolean with the same spelling stay distinct.
        let distinct = DecisionQuestion::choice(
            "x",
            vec![DecisionOption::text("true"), DecisionOption::boolean(true)],
        )
        .unwrap();
        assert_eq!(distinct.choices().unwrap().len(), 2);

        let max = DecisionQuestion::choice(
            "x",
            (0..255)
                .map(|index| DecisionOption::text(index.to_string()))
                .collect(),
        )
        .unwrap();
        assert_eq!(max.choices().unwrap().len(), 255);
        assert!(DecisionQuestion::choice(
            "x",
            (0..256)
                .map(|index| DecisionOption::text(index.to_string()))
                .collect(),
        )
        .is_err());
    }

    #[test]
    fn score_questions_require_at_least_one_level() {
        assert_eq!(
            DecisionQuestion::score("x", Vec::new())
                .unwrap_err()
                .message(),
            "levels must not be empty"
        );
        let question = score_question();
        assert_eq!(question.instructions(), "Rate");
        assert_eq!(question.levels().unwrap().len(), 2);
        assert!(question.choices().is_none());
        assert!(predicate().levels().is_none());
        assert_eq!(choice_question().instructions(), "Select");
    }

    #[test]
    fn requests_enforce_the_documented_boundaries() {
        let input = DecisionInput::text("Input").unwrap();

        assert_eq!(
            DecisionRequest::new(input.clone(), None, BTreeMap::new(), None)
                .unwrap_err()
                .message(),
            "questions must contain 1 to 400 entries"
        );

        let too_many: BTreeMap<String, DecisionQuestion> = (0..401)
            .map(|index| (index.to_string(), predicate()))
            .collect();
        assert!(DecisionRequest::new(input.clone(), None, too_many, None).is_err());

        let blank_name = questions(&[(" ", predicate())]);
        assert_eq!(
            DecisionRequest::new(input.clone(), None, blank_name, None)
                .unwrap_err()
                .message(),
            "question names must not be blank"
        );
        assert_eq!(
            DecisionRequest::new(
                input.clone(),
                Some(" ".to_string()),
                questions(&[("a", predicate())]),
                None
            )
            .unwrap_err()
            .message(),
            "model must not be blank"
        );
        assert_eq!(
            DecisionRequest::new(
                input,
                None,
                questions(&[("a", predicate())]),
                Some(" ".to_string())
            )
            .unwrap_err()
            .message(),
            "promptCacheKey must not be blank"
        );
    }

    #[test]
    fn builder_rejects_missing_input_duplicate_names_and_invalid_chains() {
        assert_eq!(
            DecisionRequest::builder()
                .input_text("x")
                .build()
                .unwrap_err()
                .message(),
            "questions must contain 1 to 400 entries",
            "Java builds with an empty question map and rejects it in the record"
        );
        assert_eq!(
            DecisionRequest::builder()
                .question("a", predicate())
                .build()
                .unwrap_err()
                .message(),
            "input must not be null"
        );
        assert_eq!(
            DecisionRequest::builder()
                .input_text("x")
                .question("a", predicate())
                .question("a", predicate())
                .build()
                .unwrap_err()
                .message(),
            "question names must be unique"
        );

        let request = DecisionRequest::builder()
            .input_text("The screen is broken")
            .model("override-model")
            .prompt_cache_key("tenant-42")
            .question("damaged", predicate())
            .question("route", choice_question())
            .question("severity", score_question())
            .build()
            .unwrap();
        assert_eq!(request.input().text_value(), Some("The screen is broken"));
        assert_eq!(request.model(), Some("override-model"));
        assert_eq!(request.prompt_cache_key(), Some("tenant-42"));
        assert_eq!(request.questions().len(), 3);
    }

    #[test]
    fn answers_expose_their_shape() {
        let refusal = DecisionAnswer::Refusal;
        assert!(refusal.is_refusal());
        let predicate = DecisionAnswer::Predicate { probability: 0.0 };
        assert!(!predicate.is_refusal());
        assert_eq!(DecisionUsage::default().input_tokens, None);
        assert_eq!(DecisionValue::from("a"), DecisionValue::text("a"));
        assert_eq!(DecisionValue::from(true), DecisionValue::boolean(true));
        assert_ne!(DecisionValue::text("true"), DecisionValue::boolean(true));
    }
}
