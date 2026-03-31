use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use maud::{PreEscaped, html};
use tracing::error;

use crate::{BotData, PollSelection, crous::fetch_restaurant_menus};

pub async fn create_poll(data: BotData) -> RoomMessageEventContent {
    let dishes = match fetch_restaurant_menus().await {
        Ok(d) => d,
        Err(e) => {
            error!("error fetching crous menus: {}", e);
            vec![]
        }
    };
    let html_content = html! {
        h1 { "Miam Daily Poll" }
        p { (PollSelection::LabriWithFood.as_emoji()) (PreEscaped("&nbsp;I will eat at LaBRI and already have my food")) }
        @if !data.labri_with_food.is_empty() {
            p {
                @for (user_id, _) in &data.labri_with_food {
                    a href=(user_id.matrix_to_uri()) { (user_id.localpart())}
                }
            }
        }
        p { (PollSelection::LabriBuyFood.as_emoji()) (PreEscaped("&nbsp;I will eat at LaBRI but need to pickup some food around 12")) }
        @if !data.labri_buy_food.is_empty() {
            p {
                @for (user_id, _) in &data.labri_buy_food {
                    a href=(user_id.matrix_to_uri()) { (user_id.localpart())}
                }
            }
        }
        p {
            (PollSelection::Crous.as_emoji()) (PreEscaped("&nbsp;I will eat at the Haut Carré restaurant"))
            br;
            @for dish in dishes.iter().filter(|d|!d.food.contains("menu non communiqué")) {
                (PreEscaped("&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;"))
                i { (dish.style) ": " (dish.food) }
                br;
            }
        }

        @if !data.crous.is_empty() {
            p {
                @for (user_id, _) in &data.crous {
                    a href=(user_id.matrix_to_uri()) { (user_id.localpart())}
                }
            }
        }
        p { (PollSelection::Cnrs.as_emoji()) (PreEscaped("&nbsp;I will eat at the CNRS restaurant")) }
        @if !data.cnrs.is_empty() {
            p {
                @for (user_id, _) in &data.cnrs {
                    a href=(user_id.matrix_to_uri()) { (user_id.localpart())}
                }
            }
        }
        p { (PollSelection::Other.as_emoji()) (PreEscaped("&nbsp;I will eat somewhere else")) }
        @if !data.other.is_empty() {
            p {
                @for (user_id, _) in &data.other {
                    a href=(user_id.matrix_to_uri()) { (user_id.localpart())}
                }
            }
        }
    };
    RoomMessageEventContent::text_html("the poll, but your client doesn't render html", html_content)
}

