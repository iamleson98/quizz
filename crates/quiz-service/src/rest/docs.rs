//! The OpenAPI document, generated from the handlers and types.

use super::{dto, handlers};
use crate::error::{ErrorBody, FieldError};
use crate::model::QuestionKind;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

struct Bearer;

impl Modify for Bearer {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "A ticket signed by the LMS server (role teacher or host), \
                         sent as `Authorization: Bearer <ticket>`.",
                    ))
                    .build(),
            ),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Quiz service: the bank",
        description = "Authoring and listing quizzes, keeping them, and reading the results of \
                       finished games. The live game itself is the websocket in PROTOCOL.md.",
    ),
    paths(
        handlers::list_quizzes,
        handlers::create_quiz,
        handlers::get_quiz,
        handlers::update_quiz,
        handlers::keep_quiz,
        handlers::delete_quiz,
        handlers::list_quiz_games,
    ),
    components(schemas(
        ErrorBody, FieldError, QuestionKind, dto::QuizSummary, dto::QuizList, dto::Quiz,
        dto::QuestionOut, dto::OptionOut, dto::QuestionIn, dto::OptionIn, dto::CreateQuizBody,
        dto::UpdateQuizBody, dto::KeptBody, dto::GameResult, dto::PlayerResult,
        dto::QuestionResult, dto::GameMode,
    )),
    modifiers(&Bearer),
    tags((name = "quizzes", description = "The teacher's quiz bank"))
)]
pub struct ApiDoc;

/// The document as committed in `openapi.json`.
pub fn openapi_json() -> String {
    let mut json = ApiDoc::openapi()
        .to_pretty_json()
        .expect("the OpenAPI document serialises");
    json.push('\n');
    json
}
