// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Server-Sent Events stream for the in-browser UI.
//!
//! `EventSource("/v1/events?token=...")` subscribes to the user's
//! fan-out channel and receives every artifact-create / artifact-update
//! / artifact-delete as a typed SSE event. Browser only needs the
//! native `EventSource` API (Chrome / Firefox / Safari since forever).

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::{Extension, State},
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::Stream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::auth::UserId;
use crate::state::{AppState, BroadcastMsg};

pub async fn events(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.broadcast_tx.subscribe();
    let user_id = user.0;

    let stream = BroadcastStream::new(rx).filter_map(move |msg| match msg {
        Ok(BroadcastMsg { user_id: msg_user, event, payload }) if msg_user == user_id => {
            // SSE: `event:` line + `data:` JSON line. Browser-side
            // listeners use addEventListener("artifact-update", ...).
            let body = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
            let ev = Event::default().event(event).data(body);
            Some(Ok(ev))
        }
        Ok(_) => None, // event for a different user; filter out
        Err(BroadcastStreamRecvError::Lagged(_)) => {
            // Tell the browser to re-fetch — it dropped some events.
            Some(Ok(Event::default().event("lagged").data("{}")))
        }
    });

    Sse::new(stream).keep_alive(
        // A ping every 15 s keeps the TCP connection healthy through
        // proxies and idle-timeout middleware.
        KeepAlive::new().interval(Duration::from_secs(15)).text("ping"),
    )
}
