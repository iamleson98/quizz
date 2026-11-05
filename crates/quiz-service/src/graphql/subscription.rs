//! GraphQL Subscription root: live leaderboard pushes over the
//! `graphql-transport-ws` protocol (see `/graphql/ws`).

use super::types::*;
use crate::state::AppState;
use async_graphql::*;
use futures::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

pub struct SubscriptionRoot;

#[Subscription]
impl SubscriptionRoot {
    /// Fresh top-10 leaderboard snapshot every time anyone submits an
    /// attempt for this quiz.
    async fn leaderboard_updated(
        &self,
        ctx: &Context<'_>,
        quiz_id: ID,
    ) -> impl futures::Stream<Item = FieldResult<LeaderboardUpdateG>> {
        let state = match ctx.data::<AppState>() {
            Ok(s) => s.clone(),
            Err(e) => {
                return futures::stream::once(async move { Err(e) }).boxed();
            }
        };
        let qid = match parse_id(&quiz_id) {
            Ok(v) => v,
            Err(e) => {
                return futures::stream::once(async move { Err(e.into()) }).boxed();
            }
        };

        let rx = state.leaderboard_tx.subscribe();
        BroadcastStream::new(rx)
            .filter_map(move |event| {
                let Ok(event) = event else {
                    return std::future::ready(None);
                };
                if event.quiz_id != qid {
                    return std::future::ready(None);
                }
                std::future::ready(Some(Ok(LeaderboardUpdateG::from(event))))
            })
            .boxed()
    }
}
